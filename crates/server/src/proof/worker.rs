//! Per-zkVM worker loop that processes proof requests sequentially within a single backend, with
//! configurable timeout and graceful cancellation on shutdown.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::{sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, error, info, info_span, record_all};
use zkboost_types::{Hash256, ProofType};

use crate::{
    dashboard::DashboardMessage,
    proof::{input::NewPayloadRequestWithWitness, zkvm::zkVMInstance},
};

/// Input sent to a per-zkVM worker for proof generation.
pub(crate) struct WorkerInput {
    pub(crate) payload: Arc<NewPayloadRequestWithWitness>,
    pub(crate) span: Span,
}

/// Output returned by a worker after a proof attempt.
#[derive(Debug)]
pub(crate) struct WorkerOutput {
    pub(crate) new_payload_request_root: Hash256,
    pub(crate) block_hash: Hash256,
    pub(crate) block_number: u64,
    pub(crate) proof_type: ProofType,
    pub(crate) proof_result: ProofResult,
    pub(crate) duration: Duration,
}

/// Result of a single proof generation attempt.
#[derive(Debug)]
pub(crate) enum ProofResult {
    /// Proof generated successfully.
    Ok(Bytes),
    /// Proof generation failed with an error message.
    Err(String),
    /// Proof generation exceeded the configured timeout.
    Timeout,
}

/// Runs a per-zkVM worker loop that processes proof requests sequentially.
pub(crate) async fn run_worker(
    zkvm: zkVMInstance,
    shutdown: CancellationToken,
    mut worker_input_rx: mpsc::Receiver<WorkerInput>,
    worker_output_tx: mpsc::Sender<WorkerOutput>,
    dashboard_service_tx: mpsc::Sender<DashboardMessage>,
    proof_timeout: Duration,
) {
    let proof_type = zkvm.proof_type();
    let otel_name = format!("prove/{proof_type}");

    info!(%proof_type, "zkvm worker started");

    loop {
        let input = tokio::select! {
            biased;

            _ = shutdown.cancelled() => break,

            input = worker_input_rx.recv() => match input {
                Some(input) => input,
                None => break,
            },
        };

        let new_payload_request_root = input.payload.root();
        let block_hash = input.payload.block_hash();
        let block_number = input.payload.block_number();

        info!(%block_hash, %proof_type, "proving");

        let span = info_span!(
            parent: input.span,
            "prove",
            otel.name = otel_name,
            otel.status_code = tracing::field::Empty,
            error_reason = tracing::field::Empty,
        );

        let _ =
            dashboard_service_tx.try_send(DashboardMessage::prove_start(block_hash, proof_type));

        let start = Instant::now();
        let proof_result = match timeout(proof_timeout, zkvm.prove(&input.payload))
            .instrument(span.clone())
            .await
        {
            Ok(Ok(proof)) => ProofResult::Ok(Bytes::from(proof)),
            Ok(Err(error)) => ProofResult::Err(error.to_string()),
            Err(_) => ProofResult::Timeout,
        };
        let duration = start.elapsed();

        match &proof_result {
            ProofResult::Ok(_) => {}
            ProofResult::Err(error) => {
                record_all!(&span, otel.status_code = "ERROR", error_reason = error)
            }
            ProofResult::Timeout => {
                record_all!(&span, otel.status_code = "ERROR", error_reason = "timeout")
            }
        }

        if let Err(error) = worker_output_tx
            .send(WorkerOutput {
                new_payload_request_root,
                block_hash,
                block_number,
                proof_type,
                proof_result,
                duration,
            })
            .await
        {
            error!(%block_hash, %proof_type, %error, "worker output send failed");
        }
    }

    info!(%proof_type, "zkvm worker stopped");
}

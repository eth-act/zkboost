//! Per-zkVM worker loop that processes proof requests sequentially, with a timeout and shutdown.

use std::{sync::Arc, time::Instant};

use tokio::{sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, error, info, info_span, record_all};
use zkboost_types::ProofType;

use crate::{
    dashboard::DashboardMessage,
    metrics,
    proof::{
        input::{NewPayloadRequestMeta, StatelessInput},
        zkvm::zkVMInstance,
    },
};

/// Input sent to a per-zkVM worker for proof generation.
pub(crate) struct WorkerInput {
    /// The zkVM input of the payload.
    pub(crate) stateless_input: Arc<StatelessInput>,
    /// The request span that the prove span joins.
    pub(crate) span: Span,
    /// When the input was dispatched into the worker channel. The queue wait ends at dequeue.
    pub(crate) queued_at: Instant,
}

/// Output returned by a worker after a proof attempt.
#[derive(Debug)]
pub(crate) struct WorkerOutput {
    /// The payload metadata of the attempt.
    pub(crate) payload_meta: NewPayloadRequestMeta,
    /// Proof type of the attempt.
    pub(crate) proof_type: ProofType,
    /// Result of the attempt.
    pub(crate) proof_result: ProofResult,
}

/// Result of a single proof generation attempt.
#[derive(Debug)]
pub(crate) enum ProofResult {
    /// Proof generated successfully.
    Ok(Vec<u8>),
    /// Proof generation failed with an error message.
    Err(String),
    /// Proof generation exceeded the configured timeout.
    Timeout,
}

/// Runs the worker loop and sends every attempt to the worker output channel.
pub(crate) async fn run_worker(
    zkvm: zkVMInstance,
    shutdown: CancellationToken,
    mut worker_input_rx: mpsc::Receiver<WorkerInput>,
    worker_output_tx: mpsc::Sender<WorkerOutput>,
    dashboard_service_tx: mpsc::Sender<DashboardMessage>,
) {
    let proof_type = zkvm.proof_type();
    let proof_timeout = zkvm.proof_timeout();
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

        let payload_meta = input.stateless_input.payload_meta();

        let queue_wait = input.queued_at.elapsed();
        metrics::record_queue_wait(proof_type, queue_wait);

        info!(block_hash = %payload_meta.block_hash, %proof_type, "proving");

        let span = info_span!(
            parent: &input.span,
            "prove",
            otel.name = otel_name,
            new_payload_request_root = %payload_meta.new_payload_request_root,
            otel.status_code = tracing::field::Empty,
            error_reason = tracing::field::Empty,
            // Recorded by the cluster backend once the cluster assigns a job id.
            job_id = tracing::field::Empty,
        );

        let _ = dashboard_service_tx.try_send(DashboardMessage::prove_start(
            payload_meta.block_hash,
            proof_type,
        ));

        let start = Instant::now();
        let proof_result = match timeout(proof_timeout, zkvm.prove(&input.stateless_input, &span))
            .instrument(span.clone())
            .await
        {
            Ok(Ok(proof)) => ProofResult::Ok(proof),
            Ok(Err(error)) => ProofResult::Err(format!("{error:#}")),
            Err(_) => ProofResult::Timeout,
        };
        let duration = start.elapsed();

        let (status, proof_size) = match &proof_result {
            ProofResult::Ok(proof) => ("success", proof.len()),
            ProofResult::Err(error) => {
                record_all!(&span, otel.status_code = "ERROR", error_reason = error);
                ("error", 0)
            }
            ProofResult::Timeout => {
                record_all!(&span, otel.status_code = "ERROR", error_reason = "timeout");
                ("timeout", 0)
            }
        };
        metrics::record_prove(proof_type, status, duration, proof_size);
        let _ = dashboard_service_tx.try_send(DashboardMessage::prove_end(
            payload_meta.block_hash,
            proof_type,
            &proof_result,
        ));

        if let Err(error) = worker_output_tx
            .send(WorkerOutput {
                payload_meta,
                proof_type,
                proof_result,
            })
            .await
        {
            error!(block_hash = %payload_meta.block_hash, %proof_type, %error, "worker output send failed");
        }
    }

    info!(%proof_type, "zkvm worker stopped");
}

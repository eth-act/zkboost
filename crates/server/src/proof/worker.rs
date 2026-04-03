//! Per-zkVM worker loop that processes proof requests sequentially within a single backend, with
//! configurable timeout and graceful cancellation on shutdown.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::{sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::info;
use zkboost_types::{Hash256, ProofType};

use crate::proof::{input::NewPayloadRequestWithWitness, zkvm::zkVMInstance};

/// Input sent to a per-zkVM worker for proof generation.
pub(crate) struct WorkerInput {
    pub(crate) payload: Arc<NewPayloadRequestWithWitness>,
}

/// Output returned by a worker after a proof attempt.
#[derive(Debug)]
pub(crate) struct WorkerOutput {
    pub(crate) new_payload_request_root: Hash256,
    pub(crate) proof_type: ProofType,
    pub(crate) proof_result: ProofResult,
    pub(crate) duration: Duration,
}

/// Result of a single proof generation attempt.
#[derive(Debug)]
pub(crate) enum ProofResult {
    /// Proof generated successfully.
    Success(Bytes),
    /// Proof generation failed with an error message.
    Failure(String),
    /// Proof generation exceeded the configured timeout.
    Timeout,
}

/// Runs a per-zkVM worker loop that processes proof requests sequentially.
pub(crate) async fn run_worker(
    zkvm: zkVMInstance,
    shutdown: CancellationToken,
    mut worker_input_rx: mpsc::Receiver<WorkerInput>,
    worker_output_tx: mpsc::Sender<WorkerOutput>,
    proof_timeout: Duration,
) {
    let proof_type = zkvm.proof_type();
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

        info!(
            %new_payload_request_root,
            %proof_type,
            "proving"
        );

        let start = Instant::now();
        let proof_result = match timeout(proof_timeout, zkvm.prove(&input.payload)).await {
            Ok(Ok(proof)) => ProofResult::Success(Bytes::from(proof)),
            Ok(Err(error)) => ProofResult::Failure(error.to_string()),
            Err(_) => ProofResult::Timeout,
        };
        let duration = start.elapsed();

        let _ = worker_output_tx
            .send(WorkerOutput {
                new_payload_request_root,
                proof_type,
                proof_result,
                duration,
            })
            .await;
    }

    info!(%proof_type, "zkvm worker stopped");
}

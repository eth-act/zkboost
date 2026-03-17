//! Per-zkVM worker loop that processes proof requests sequentially within a single backend, with
//! configurable timeout and graceful cancellation on shutdown.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use ere_zkvm_interface::Proof;
use tokio::{sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use zkboost_types::{Hash256, ProofType};

use crate::{
    metrics::record_prove,
    proof::{input::NewPayloadRequestWithWitness, zkvm::zkVMInstance},
};

/// Input sent to a per-zkVM worker for proof generation.
#[allow(missing_debug_implementations)]
pub(crate) struct WorkerInput {
    pub(crate) payload: Arc<NewPayloadRequestWithWitness>,
    pub(crate) new_payload_request_root: Hash256,
    pub(crate) proof_type: ProofType,
}

/// Output returned by a worker after a proof attempt.
#[derive(Debug)]
pub(crate) struct WorkerOutput {
    pub(crate) new_payload_request_root: Hash256,
    pub(crate) proof_type: ProofType,
    pub(crate) proof_result: ProofResult,
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
    info!(proof_type = %proof_type, "zkvm worker started");

    loop {
        let input = tokio::select! {
            biased;

            _ = shutdown.cancelled() => break,

            input = worker_input_rx.recv() => match input {
                Some(input) => input,
                None => break,
            },
        };

        let new_payload_request_root = input.new_payload_request_root;

        info!(
            new_payload_request_root = %new_payload_request_root,
            proof_type = %proof_type,
            "proving"
        );

        let start = Instant::now();

        let proof_result = match timeout(proof_timeout, zkvm.prove(&input.payload)).await {
            Err(_) => ProofResult::Timeout,
            Ok(Ok((_, Proof::Compressed(proof), _))) => ProofResult::Success(Bytes::from(proof)),
            Ok(Ok((_, proof, _))) => {
                ProofResult::Failure(format!("unexpected proof kind: {:?}", proof.kind()))
            }
            Ok(Err(e)) => ProofResult::Failure(format!("{e}")),
        };

        let duration = start.elapsed();
        let (success, proof_size) = match &proof_result {
            ProofResult::Success(bytes) => (true, bytes.len()),
            _ => (false, 0),
        };
        record_prove(proof_type, success, duration, proof_size);

        match &proof_result {
            ProofResult::Success(proof) => {
                info!(
                    new_payload_request_root = %new_payload_request_root,
                    proof_type = %proof_type,
                    proof_size = proof.len(),
                    "proof generated"
                );
            }
            ProofResult::Failure(err) => {
                error!(
                    new_payload_request_root = %new_payload_request_root,
                    proof_type = %proof_type,
                    error = %err,
                    "proof generation failed"
                );
            }
            ProofResult::Timeout => {
                error!(
                    new_payload_request_root = %new_payload_request_root,
                    proof_type = %proof_type,
                    "proof generation timed out"
                );
            }
        }

        let _ = worker_output_tx
            .send(WorkerOutput {
                new_payload_request_root,
                proof_type: input.proof_type,
                proof_result,
            })
            .await;
    }

    info!(proof_type = %proof_type, "zkvm worker stopped");
}

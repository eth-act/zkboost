//! Proof generation service managing the proof lifecycle: pending (waiting for witness), enqueued
//! (dispatched to per-zkVM worker), and completed (cached in LRU, broadcast via SSE).

pub mod input;
pub mod worker;
pub mod zkvm;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_genesis::ChainConfig;
use bytes::Bytes;
use input::NewPayloadRequestWithWitness;
use lru::LruCache;
use stateless::ExecutionWitness;
use tokio::{
    sync::{RwLock, broadcast, mpsc, mpsc::error::TrySendError},
    time::interval,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use worker::WorkerInput;
use zkboost_types::{
    FailureReason, Hash256, MainnetEthSpec, NewPayloadRequest, ProofComplete, ProofEvent,
    ProofFailure, ProofType, TreeHash,
};

use crate::{
    proof::worker::{ProofResult, WorkerOutput},
    witness::WitnessServiceMessage,
};

const CLEANUP_INTERVAL: Duration = Duration::from_secs(12);

/// Messages consumed by the proof service event loop.
#[derive(Debug)]
pub(crate) enum ProofServiceMessage {
    /// A new proof has been requested for the given payload and proof types.
    RequestProof {
        new_payload_request: Arc<NewPayloadRequest<MainnetEthSpec>>,
        proof_types: Vec<ProofType>,
    },
    /// An execution witness has been fetched and is ready for proof generation.
    WitnessAvailable {
        block_hash: Hash256,
        witness: Arc<ExecutionWitness>,
    },
}

struct PendingRequest {
    new_payload_request: Arc<NewPayloadRequest<MainnetEthSpec>>,
    new_payload_request_root: Hash256,
    proof_type: ProofType,
    created_at: Instant,
}

/// Manages proof lifecycle: pending, enqueued, and completed proof requests.
#[allow(missing_debug_implementations)]
pub(crate) struct ProofService {
    chain_config: Arc<ChainConfig>,
    completed_proofs: Arc<RwLock<LruCache<(Hash256, ProofType), Bytes>>>,
    proof_event_tx: broadcast::Sender<ProofEvent>,
    witness_service_tx: mpsc::Sender<WitnessServiceMessage>,
    witness_timeout: Duration,
    proof_timeout: Duration,
    pending: HashMap<Hash256, Vec<PendingRequest>>,
    in_flight: HashSet<(Hash256, ProofType)>,
}

impl ProofService {
    /// Creates a new proof service with the given dependencies.
    pub(crate) fn new(
        chain_config: Arc<ChainConfig>,
        completed_proofs: Arc<RwLock<LruCache<(Hash256, ProofType), Bytes>>>,
        proof_event_tx: broadcast::Sender<ProofEvent>,
        witness_service_tx: mpsc::Sender<WitnessServiceMessage>,
        witness_timeout: Duration,
        proof_timeout: Duration,
    ) -> Self {
        Self {
            chain_config,
            completed_proofs,
            proof_event_tx,
            witness_service_tx,
            witness_timeout,
            proof_timeout,
            pending: HashMap::new(),
            in_flight: HashSet::new(),
        }
    }

    /// Runs the proof service event loop until shutdown is signalled.
    pub(crate) async fn run(
        mut self,
        shutdown: CancellationToken,
        mut proof_service_rx: mpsc::Receiver<ProofServiceMessage>,
        mut worker_output_rx: mpsc::Receiver<WorkerOutput>,
        worker_input_txs: HashMap<ProofType, mpsc::Sender<WorkerInput>>,
    ) {
        let mut cleanup_interval = interval(CLEANUP_INTERVAL);

        info!("proof service started");

        loop {
            tokio::select! {
                biased;

                _ = shutdown.cancelled() => {
                    info!("proof service shutting down");
                    drop(worker_input_txs);
                    break;
                }

                Some(output) = worker_output_rx.recv() => self.handle_worker_output(output).await,

                Some(msg) = proof_service_rx.recv() => self.handle_message(msg, &worker_input_txs).await,

                _ = cleanup_interval.tick() => self.cleanup_stale_requests(),

                else => break,
            }
        }

        info!("proof service stopped");
    }

    fn cleanup_stale_requests(&mut self) {
        let witness_timeout = self.witness_timeout;
        let in_flight = &mut self.in_flight;
        let proof_event_tx = &self.proof_event_tx;
        self.pending.retain(|block_hash, entries| {
            entries.retain(|request| {
                let is_stale = request.created_at.elapsed() >= witness_timeout;
                if is_stale {
                    warn!(
                        block_hash = %block_hash,
                        proof_type = %request.proof_type,
                        elapsed_secs = request.created_at.elapsed().as_secs(),
                        "pending request timed out"
                    );
                    in_flight.remove(&(request.new_payload_request_root, request.proof_type));
                    let _ = proof_event_tx.send(
                        ProofFailure {
                            new_payload_request_root: request.new_payload_request_root,
                            proof_type: request.proof_type,
                            reason: FailureReason::WitnessTimeout,
                            error: format!(
                                "witness timeout after {} seconds",
                                witness_timeout.as_secs()
                            ),
                        }
                        .into(),
                    );
                }
                // Removes timeout requests
                !is_stale
            });
            // Removes empty groups
            !entries.is_empty()
        });
    }

    async fn handle_worker_output(&mut self, output: WorkerOutput) {
        let new_payload_request_root = output.new_payload_request_root;
        let proof_type = output.proof_type;
        self.in_flight
            .remove(&(new_payload_request_root, proof_type));

        match output.proof_result {
            ProofResult::Success(proof) => {
                self.completed_proofs
                    .write()
                    .await
                    .put((new_payload_request_root, proof_type), proof);
                let _ = self.proof_event_tx.send(
                    ProofComplete {
                        new_payload_request_root,
                        proof_type,
                    }
                    .into(),
                );
            }
            ProofResult::Failure(error) => {
                let _ = self.proof_event_tx.send(
                    ProofFailure {
                        new_payload_request_root,
                        proof_type,
                        reason: FailureReason::ProvingError,
                        error,
                    }
                    .into(),
                );
            }
            ProofResult::Timeout => {
                let _ = self.proof_event_tx.send(
                    ProofFailure {
                        new_payload_request_root,
                        proof_type,
                        reason: FailureReason::ProvingTimeout,
                        error: format!(
                            "proving timeout after {} seconds",
                            self.proof_timeout.as_secs()
                        ),
                    }
                    .into(),
                );
            }
        }
    }

    async fn handle_message(
        &mut self,
        message: ProofServiceMessage,
        worker_input_txs: &HashMap<ProofType, mpsc::Sender<WorkerInput>>,
    ) {
        match message {
            ProofServiceMessage::RequestProof {
                new_payload_request,
                proof_types,
            } => {
                let block_hash = new_payload_request.block_hash();
                let new_payload_request_root = new_payload_request.tree_hash_root();
                let mut fetch_witness = false;

                for proof_type in proof_types {
                    {
                        let cache = self.completed_proofs.read().await;
                        if cache.contains(&(new_payload_request_root, proof_type)) {
                            debug!(
                                %new_payload_request_root,
                                proof_type = %proof_type,
                                "proof already completed"
                            );
                            continue;
                        }
                    }

                    if !self
                        .in_flight
                        .insert((new_payload_request_root, proof_type))
                    {
                        debug!(
                            %new_payload_request_root,
                            proof_type = %proof_type,
                            "duplicate proof request"
                        );
                        continue;
                    }

                    debug!(
                        %new_payload_request_root,
                        %block_hash,
                        proof_type = %proof_type,
                        "new proof request"
                    );

                    self.pending
                        .entry(block_hash)
                        .or_default()
                        .push(PendingRequest {
                            new_payload_request: new_payload_request.clone(),
                            new_payload_request_root,
                            proof_type,
                            created_at: Instant::now(),
                        });
                    fetch_witness = true;
                }

                if fetch_witness
                    && let Err(error) = self
                        .witness_service_tx
                        .send(WitnessServiceMessage::FetchWitness { block_hash })
                        .await
                {
                    error!(error = %error, "witness request send failed");
                    if let Some(entries) = self.pending.remove(&block_hash) {
                        for request in &entries {
                            self.in_flight
                                .remove(&(request.new_payload_request_root, request.proof_type));
                            let _ = self.proof_event_tx.send(
                                ProofFailure {
                                    new_payload_request_root: request.new_payload_request_root,
                                    proof_type: request.proof_type,
                                    reason: FailureReason::ProvingError,
                                    error: format!("witness service unavailable: {error}"),
                                }
                                .into(),
                            );
                        }
                    }
                }
            }
            ProofServiceMessage::WitnessAvailable {
                block_hash,
                witness,
            } => {
                let Some(pending) = self.pending.remove(&block_hash) else {
                    return;
                };

                info!(
                    %block_hash,
                    count = pending.len(),
                    "dispatching pending requests"
                );

                let input = match NewPayloadRequestWithWitness::new(
                    &pending[0].new_payload_request,
                    witness,
                    self.chain_config.clone(),
                ) {
                    Ok(input) => Arc::new(input),
                    Err(e) => {
                        for request in &pending {
                            self.in_flight
                                .remove(&(request.new_payload_request_root, request.proof_type));
                            let _ = self.proof_event_tx.send(
                                ProofFailure {
                                    new_payload_request_root: request.new_payload_request_root,
                                    proof_type: request.proof_type,
                                    reason: FailureReason::ProvingError,
                                    error: format!("input construction failed: {e}"),
                                }
                                .into(),
                            );
                        }
                        return;
                    }
                };

                for request in pending {
                    dispatch_to_worker(
                        worker_input_txs,
                        &self.proof_event_tx,
                        &mut self.in_flight,
                        request.proof_type,
                        input.clone(),
                    );
                }
            }
        }
    }
}

fn dispatch_to_worker(
    worker_input_txs: &HashMap<ProofType, mpsc::Sender<WorkerInput>>,
    proof_event_tx: &broadcast::Sender<ProofEvent>,
    in_flight: &mut HashSet<(Hash256, ProofType)>,
    proof_type: ProofType,
    payload: Arc<NewPayloadRequestWithWitness>,
) {
    let new_payload_request_root = payload.root();

    let Some(tx) = worker_input_txs.get(&proof_type) else {
        in_flight.remove(&(new_payload_request_root, proof_type));
        let _ = proof_event_tx.send(
            ProofFailure {
                new_payload_request_root,
                proof_type,
                reason: FailureReason::ProvingError,
                error: format!("no zkVM worker for proof type '{proof_type}'"),
            }
            .into(),
        );
        return;
    };

    let worker_input = WorkerInput {
        payload,
        new_payload_request_root,
        proof_type,
    };

    match tx.try_send(worker_input) {
        Ok(()) => {
            debug!(proof_type = %proof_type, "proof dispatched");
        }
        Err(error) => {
            let reason = match &error {
                TrySendError::Full(_) => "worker channel full",
                TrySendError::Closed(_) => "worker channel closed",
            };
            in_flight.remove(&(new_payload_request_root, proof_type));
            let _ = proof_event_tx.send(
                ProofFailure {
                    new_payload_request_root,
                    proof_type,
                    reason: FailureReason::ProvingError,
                    error: format!("dispatch failed: {reason}"),
                }
                .into(),
            );
        }
    }
}

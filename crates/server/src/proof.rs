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
    ProofFailure, ProofType,
};

use crate::{
    metrics::record_prove,
    proof::worker::{ProofResult, WorkerOutput},
    witness::WitnessServiceMessage,
};

const CLEANUP_INTERVAL: Duration = Duration::from_secs(12);

/// Messages consumed by the proof service event loop.
#[derive(Debug)]
pub(crate) enum ProofServiceMessage {
    /// A new proof has been requested for the given payload and proof types.
    RequestProof {
        new_payload_request_root: Hash256,
        new_payload_request: Arc<NewPayloadRequest<MainnetEthSpec>>,
        proof_types: HashSet<ProofType>,
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
    proof_types: HashSet<ProofType>,
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
    pending: HashMap<Hash256, PendingRequest>,
    requested: HashSet<(Hash256, ProofType)>,
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
            requested: HashSet::new(),
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
        let requested = &mut self.requested;
        let proof_event_tx = &self.proof_event_tx;
        self.pending.retain(|block_hash, request| {
            let is_stale = request.created_at.elapsed() >= witness_timeout;
            if is_stale {
                for &proof_type in &request.proof_types {
                    warn!(
                        block_hash = %block_hash,
                        %proof_type,
                        elapsed_secs = request.created_at.elapsed().as_secs(),
                        "pending request timed out"
                    );
                    requested.remove(&(request.new_payload_request_root, proof_type));
                    let _ = proof_event_tx.send(
                        ProofFailure {
                            new_payload_request_root: request.new_payload_request_root,
                            proof_type,
                            reason: FailureReason::WitnessTimeout,
                            error: format!(
                                "witness timeout after {} seconds",
                                witness_timeout.as_secs()
                            ),
                        }
                        .into(),
                    );
                    record_prove(proof_type, "timeout", Duration::ZERO, 0);
                }
            }
            !is_stale
        });
    }

    async fn handle_worker_output(&mut self, output: WorkerOutput) {
        let new_payload_request_root = output.new_payload_request_root;
        let proof_type = output.proof_type;
        self.requested
            .remove(&(new_payload_request_root, proof_type));

        let proof_event = match output.proof_result {
            ProofResult::Success(proof) => {
                self.completed_proofs
                    .write()
                    .await
                    .put((new_payload_request_root, proof_type), proof);
                ProofComplete {
                    new_payload_request_root,
                    proof_type,
                }
                .into()
            }
            ProofResult::Failure(error) => ProofFailure {
                new_payload_request_root,
                proof_type,
                reason: FailureReason::ProvingError,
                error,
            }
            .into(),
            ProofResult::Timeout => ProofFailure {
                new_payload_request_root,
                proof_type,
                reason: FailureReason::ProvingTimeout,
                error: format!(
                    "proving timeout after {} seconds",
                    self.proof_timeout.as_secs()
                ),
            }
            .into(),
        };
        let _ = self.proof_event_tx.send(proof_event);
    }

    async fn handle_message(
        &mut self,
        message: ProofServiceMessage,
        worker_input_txs: &HashMap<ProofType, mpsc::Sender<WorkerInput>>,
    ) {
        match message {
            ProofServiceMessage::RequestProof {
                new_payload_request_root,
                new_payload_request,
                mut proof_types,
            } => {
                let block_hash = new_payload_request.block_hash();

                // Deduplicate
                {
                    let cache = self.completed_proofs.read().await;
                    proof_types.retain(|proof_type| {
                        if cache.contains(&(new_payload_request_root, *proof_type)) {
                            debug!(
                                %new_payload_request_root,
                                %proof_type,
                                "proof already completed"
                            );
                            return false;
                        }

                        if !self
                            .requested
                            .insert((new_payload_request_root, *proof_type))
                        {
                            debug!(
                                %new_payload_request_root,
                                %proof_type,
                                "duplicate proof request"
                            );
                            return false;
                        }

                        true
                    });
                }

                if proof_types.is_empty() {
                    return;
                }

                debug!(
                    %new_payload_request_root,
                    %block_hash,
                    ?proof_types,
                    "new proof requests"
                );

                if !self.pending.contains_key(&block_hash)
                    && let Err(error) = self
                        .witness_service_tx
                        .send(WitnessServiceMessage::FetchWitness { block_hash })
                        .await
                {
                    for &proof_type in &proof_types {
                        self.fail_request(
                            new_payload_request_root,
                            proof_type,
                            format!("witness service unavailable: {error}"),
                        );
                    }
                    error!(error = %error, "fetch witness send failed");
                    return;
                }

                self.pending
                    .entry(block_hash)
                    .and_modify(|r| {
                        r.proof_types.extend(proof_types.iter().copied());
                    })
                    .or_insert_with(|| PendingRequest {
                        new_payload_request: new_payload_request.clone(),
                        new_payload_request_root,
                        proof_types,
                        created_at: Instant::now(),
                    });
            }
            ProofServiceMessage::WitnessAvailable {
                block_hash,
                witness,
            } => {
                let Some(request) = self.pending.remove(&block_hash) else {
                    return;
                };

                info!(
                    %block_hash,
                    count = request.proof_types.len(),
                    "dispatching pending requests"
                );

                let input = match NewPayloadRequestWithWitness::new(
                    &request.new_payload_request,
                    request.new_payload_request_root,
                    witness,
                    self.chain_config.clone(),
                ) {
                    Ok(input) => Arc::new(input),
                    Err(e) => {
                        for &proof_type in &request.proof_types {
                            self.fail_request(
                                request.new_payload_request_root,
                                proof_type,
                                format!("input construction failed: {e}"),
                            );
                        }
                        return;
                    }
                };

                for proof_type in request.proof_types {
                    self.dispatch_to_worker(worker_input_txs, proof_type, input.clone());
                }
            }
        }
    }

    fn dispatch_to_worker(
        &mut self,
        worker_input_txs: &HashMap<ProofType, mpsc::Sender<WorkerInput>>,
        proof_type: ProofType,
        payload: Arc<NewPayloadRequestWithWitness>,
    ) {
        let new_payload_request_root = payload.root();

        let Some(tx) = worker_input_txs.get(&proof_type) else {
            self.fail_request(
                new_payload_request_root,
                proof_type,
                format!("no zkVM worker for proof type '{proof_type}'"),
            );
            return;
        };

        let worker_input = WorkerInput { payload };
        match tx.try_send(worker_input) {
            Ok(()) => {
                debug!(%proof_type, "proof dispatched");
            }
            Err(error) => {
                let reason = match &error {
                    TrySendError::Full(_) => "worker channel full",
                    TrySendError::Closed(_) => "worker channel closed",
                };
                self.fail_request(
                    new_payload_request_root,
                    proof_type,
                    format!("dispatch failed: {reason}"),
                );
            }
        }
    }

    fn fail_request(
        &mut self,
        new_payload_request_root: Hash256,
        proof_type: ProofType,
        error: String,
    ) {
        self.requested
            .remove(&(new_payload_request_root, proof_type));
        let _ = self.proof_event_tx.send(
            ProofFailure {
                new_payload_request_root,
                proof_type,
                reason: FailureReason::ProvingError,
                error,
            }
            .into(),
        );
        record_prove(proof_type, "error", Duration::ZERO, 0);
    }
}

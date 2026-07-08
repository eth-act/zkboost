//! Proof generation service managing the proof lifecycle: pending (waiting for witness), enqueued
//! (dispatched to per-zkVM worker), and completed (cached in LRU, broadcast via SSE).

pub mod input;
pub mod worker;
pub mod zkvm;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use ere_guests_stateless_validator_common::guest::input::ExecutionWitness;
use input::StatelessInput;
use lru::LruCache;
use tokio::sync::{RwLock, broadcast, mpsc, mpsc::error::TrySendError};
use tokio_util::sync::CancellationToken;
use tracing::{Span, debug, error, info, trace, warn};
use worker::WorkerInput;
use zkboost_types::{
    ChainConfig, FailureReason, Hash256, NewPayloadRequest, ProofComplete, ProofEvent,
    ProofFailure, ProofType,
};

use crate::{
    dashboard::DashboardMessage,
    metrics::record_prove,
    proof::worker::{ProofResult, WorkerOutput},
    witness::WitnessServiceMessage,
};

/// Messages consumed by the proof service event loop.
#[derive(Debug)]
pub(crate) enum ProofServiceMessage {
    /// A new proof has been requested for the given payload and proof types.
    RequestProof {
        new_payload_request_root: Hash256,
        new_payload_request: Arc<NewPayloadRequest>,
        chain_config: ChainConfig,
        proof_types: HashSet<ProofType>,
        span: Span,
    },
    /// An execution witness has been fetched and is ready for proof generation.
    WitnessAvailable {
        block_hash: Hash256,
        witness: Arc<ExecutionWitness>,
    },
    /// The witness service timed out fetching the witness for the given block hash.
    WitnessTimeout { block_hash: Hash256 },
    /// A witness was fetched but is incompatible with the SSZ container limit.
    WitnessIncompatible { block_hash: Hash256, error: String },
}

struct PendingRequest {
    new_payload_request: Arc<NewPayloadRequest>,
    new_payload_request_root: Hash256,
    chain_config: ChainConfig,
    proof_types: HashSet<ProofType>,
    span: Span,
}

/// Manages proof lifecycle: pending, enqueued, and completed proof requests.
pub(crate) struct ProofService {
    proof_cache: Arc<RwLock<LruCache<(Hash256, ProofType), Bytes>>>,
    proof_event_tx: broadcast::Sender<ProofEvent>,
    witness_service_tx: mpsc::Sender<WitnessServiceMessage>,
    dashboard_service_tx: mpsc::Sender<DashboardMessage>,
    pending: HashMap<Hash256, PendingRequest>,
    requested: HashSet<(Hash256, ProofType)>,
}

impl ProofService {
    /// Creates a new proof service with the given dependencies.
    pub(crate) fn new(
        proof_cache: Arc<RwLock<LruCache<(Hash256, ProofType), Bytes>>>,
        proof_event_tx: broadcast::Sender<ProofEvent>,
        witness_service_tx: mpsc::Sender<WitnessServiceMessage>,
        dashboard_service_tx: mpsc::Sender<DashboardMessage>,
    ) -> Self {
        Self {
            proof_cache,
            proof_event_tx,
            witness_service_tx,
            dashboard_service_tx,
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

                else => break,
            }
        }
    }

    async fn handle_worker_output(&mut self, output: WorkerOutput) {
        let WorkerOutput {
            new_payload_request_root,
            block_hash,
            block_number,
            proof_type,
            proof_result,
            duration,
        } = output;

        trace!(%block_hash, block_number, "received WorkerOutput");

        self.requested
            .remove(&(new_payload_request_root, proof_type));

        let dashboard_msg = DashboardMessage::prove_end(block_hash, proof_type, &proof_result);

        match proof_result {
            ProofResult::Ok(proof) => {
                let proof_size = proof.len();
                info!(%block_hash, block_number, %proof_type, proof_size, "proved");
                self.proof_cache
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
                record_prove(proof_type, "success", duration, proof_size);
            }
            ProofResult::Err(error) => {
                error!(%block_hash, block_number, %proof_type, %error, "proving failed");
                self.fail_request(
                    new_payload_request_root,
                    proof_type,
                    FailureReason::ProvingError,
                    error,
                    duration,
                );
            }
            ProofResult::Timeout => {
                error!(%block_hash, block_number, %proof_type, "proving timed out");
                self.fail_request(
                    new_payload_request_root,
                    proof_type,
                    FailureReason::ProvingTimeout,
                    format!(
                        "proving timed out after {:.02} seconds",
                        duration.as_secs_f64()
                    ),
                    duration,
                );
            }
        }

        let _ = self.dashboard_service_tx.try_send(dashboard_msg);
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
                chain_config,
                mut proof_types,
                span,
            } => {
                let block_hash = Hash256::from(new_payload_request.block_hash());
                let block_number = new_payload_request.block_number();

                trace!(%block_hash, block_number, "received ProofServiceMessage::RequestProof");

                // Deduplicate
                {
                    let cache = self.proof_cache.read().await;
                    proof_types.retain(|proof_type| {
                        if cache.contains(&(new_payload_request_root, *proof_type)) {
                            debug!(
                                %block_hash,
                                block_number,
                                %proof_type,
                                "proof cache hit"
                            );
                            return false;
                        }

                        if !self
                            .requested
                            .insert((new_payload_request_root, *proof_type))
                        {
                            debug!(
                                %block_hash,
                                block_number,
                                %proof_type,
                                "proof already requested"
                            );
                            return false;
                        }

                        true
                    });
                }

                if proof_types.is_empty() {
                    return;
                }

                info!(
                    %new_payload_request_root,
                    %block_hash,
                    block_number,
                    ?proof_types,
                    "received proof request"
                );

                let dashboard_msg =
                    DashboardMessage::request_proof(&new_payload_request, &proof_types);

                if !self.pending.contains_key(&block_hash)
                    && let Err(error) = self
                        .witness_service_tx
                        .send(WitnessServiceMessage::FetchWitness {
                            block_hash,
                            span: span.clone(),
                        })
                        .await
                {
                    error!(%block_hash, block_number, error = %error, "fetch witness send failed");
                    for &proof_type in &proof_types {
                        self.fail_request(
                            new_payload_request_root,
                            proof_type,
                            FailureReason::InternalError,
                            format!("witness service unavailable: {error}"),
                            Duration::ZERO,
                        );
                    }
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
                        chain_config,
                        proof_types,
                        span,
                    });

                let _ = self.dashboard_service_tx.try_send(dashboard_msg);
            }
            ProofServiceMessage::WitnessAvailable {
                block_hash,
                witness,
            } => {
                trace!(%block_hash, "received ProofServiceMessage::WitnessAvailable");

                let Some(request) = self.pending.remove(&block_hash) else {
                    return;
                };

                let input = match StatelessInput::new(
                    &request.new_payload_request,
                    request.new_payload_request_root,
                    &witness,
                    &request.chain_config,
                ) {
                    Ok(input) => Arc::new(input),
                    Err(e) => {
                        for &proof_type in &request.proof_types {
                            self.fail_request(
                                request.new_payload_request_root,
                                proof_type,
                                FailureReason::ProvingError,
                                format!("input construction failed: {e}"),
                                Duration::ZERO,
                            );
                        }
                        return;
                    }
                };

                for proof_type in request.proof_types {
                    self.send_worker_input(
                        worker_input_txs,
                        proof_type,
                        input.clone(),
                        request.span.clone(),
                    );
                }
            }
            ProofServiceMessage::WitnessTimeout { block_hash } => {
                trace!(%block_hash, "received ProofServiceMessage::WitnessTimeout");

                let Some(request) = self.pending.remove(&block_hash) else {
                    return;
                };
                for &proof_type in &request.proof_types {
                    warn!(%block_hash, %proof_type, "pending request witness timed out");
                    self.fail_request(
                        request.new_payload_request_root,
                        proof_type,
                        FailureReason::WitnessTimeout,
                        format!("witness timeout for block {block_hash}"),
                        Duration::ZERO,
                    );
                }
            }
            ProofServiceMessage::WitnessIncompatible { block_hash, error } => {
                trace!(%block_hash, "received ProofServiceMessage::WitnessIncompatible");

                let Some(request) = self.pending.remove(&block_hash) else {
                    return;
                };
                for &proof_type in &request.proof_types {
                    warn!(%block_hash, %proof_type, %error, "pending request witness incompatible");
                    self.fail_request(
                        request.new_payload_request_root,
                        proof_type,
                        FailureReason::ProvingError,
                        format!("witness incompatible: {error}"),
                        Duration::ZERO,
                    );
                }
            }
        }
    }

    fn send_worker_input(
        &mut self,
        worker_input_txs: &HashMap<ProofType, mpsc::Sender<WorkerInput>>,
        proof_type: ProofType,
        stateless_input: Arc<StatelessInput>,
        span: Span,
    ) {
        let new_payload_request_root = stateless_input.root();
        let block_hash = stateless_input.block_hash();
        let block_number = stateless_input.block_number();

        let Some(tx) = worker_input_txs.get(&proof_type) else {
            self.fail_request(
                new_payload_request_root,
                proof_type,
                FailureReason::InternalError,
                format!("no zkVM worker for proof type '{proof_type}'"),
                Duration::ZERO,
            );
            return;
        };

        let worker_input = WorkerInput {
            stateless_input,
            span,
        };
        match tx.try_send(worker_input) {
            Ok(()) => {
                debug!(%block_hash, block_number, %proof_type, "proof dispatched");
            }
            Err(error) => {
                let reason = match &error {
                    TrySendError::Full(_) => "worker channel full",
                    TrySendError::Closed(_) => "worker channel closed",
                };
                self.fail_request(
                    new_payload_request_root,
                    proof_type,
                    FailureReason::InternalError,
                    format!("worker input send failed: {reason}"),
                    Duration::ZERO,
                );
            }
        }
    }

    fn fail_request(
        &mut self,
        new_payload_request_root: Hash256,
        proof_type: ProofType,
        reason: FailureReason,
        error: String,
        duration: Duration,
    ) {
        self.requested
            .remove(&(new_payload_request_root, proof_type));
        let _ = self.proof_event_tx.send(
            ProofFailure {
                new_payload_request_root,
                proof_type,
                reason,
                error,
            }
            .into(),
        );
        record_prove(
            proof_type,
            match reason {
                FailureReason::WitnessTimeout | FailureReason::ProvingTimeout => "timeout",
                FailureReason::ProvingError | FailureReason::InternalError => "error",
            },
            duration,
            0,
        );
    }
}

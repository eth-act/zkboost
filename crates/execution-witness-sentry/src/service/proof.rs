//! Proof generation and submission service.
//!
//! This module provides [`ProofService`], which is responsible for coordinating proof generation
//! requests and submissions to CL clients.'
//!
//! ## Purpose
//!
//! The services send proof generation request to proof engine, and starts a http server to receive
//! proof result via webhook.
//!
//! Internally it receives messages via [`ProofServiceMessage`]:
//!
//! - [`RequestProof`] - Request proof generation for a block. Sent by [`ClEventService`] on new
//!   head events and [`BackfillService`] for gap filling.
//!
//! - [`BlockDataReady`] - Notification that EL block data is now available. Sent by
//!   [`ElDataService`] after fetching block and witness data. Triggers processing of any pending
//!   requests for that block.
//!
//! [`BackfillService`]: super::backfill::BackfillService
//! [`ClEventService`]: super::cl_event::ClEventService
//! [`ElDataService`]: super::el_data::ElDataService
//! [`RequestProof`]: ProofServiceMessage::RequestProof
//! [`BlockDataReady`]: ProofServiceMessage::BlockDataReady

use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_genesis::ChainConfig;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use lru::LruCache;
use reth_stateless::StatelessInput;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};
use zkboost_ethereum_el_input::ElInput;
use zkboost_ethereum_el_types::ElProofType;
use zkboost_types::{ProofGenId, ProofResult};

use crate::{
    BlockStorage, ClClient, ElBlockWitness, ExecutionProof, ProofEngineClient,
    config::ProofEngineConfig,
    service::{Target, is_el_data_available},
    storage::SavedProof,
};

const PENDING_PROOF_TIMEOUT: Duration = Duration::from_secs(300);
const PENDING_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);
const PROOF_SUBMISSION_MAX_RETRIES: u32 = 3;

/// Identifier for a proof consist of `block_hash` and `proof_type`.
type ProofKey = (String, ElProofType);

pub enum ProofServiceMessage {
    BlockDataReady {
        block_hash: String,
    },
    RequestProof {
        slot: u64,
        block_root: String,
        execution_block_hash: String,
        target_clients: Target<String>,
        target_proof_types: Target<ElProofType>,
    },
}

#[derive(Clone)]
struct PendingRequest {
    slot: u64,
    block_root: String,
    target_clients: Target<String>,
    created_at: Instant,
}

#[derive(Debug, Clone)]
struct PendingProof {
    proof_type: ElProofType,
    slot: u64,
    block_hash: String,
    beacon_block_root: String,
    target_clients: Target<String>,
    created_at: Instant,
    proof_gen_id: ProofGenId,
}

#[derive(Clone)]
pub struct ProofService {
    webhook_port: u16,
    proof_engine_client: Arc<ProofEngineClient>,
    chain_config: ChainConfig,
    zkvm_enabled_cl_clients: Vec<Arc<ClClient>>,
    block_cache: Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    proof_cache: Arc<Mutex<LruCache<ProofKey, SavedProof>>>,
    storage: Option<Arc<Mutex<BlockStorage>>>,
    /// Requests pending for EL data to become available.
    pending_requests: Arc<Mutex<HashMap<String, BTreeMap<ElProofType, PendingRequest>>>>,
    /// Proofs pending for proof engine to send back via webhook.
    pending_proofs: Arc<Mutex<HashMap<ProofKey, PendingProof>>>,
    /// Mapping from [`ProofGenId`] to [`ProofKey`]
    proof_gen_ids: Arc<Mutex<HashMap<ProofGenId, ProofKey>>>,
    proof_rx: Arc<Mutex<mpsc::Receiver<ProofServiceMessage>>>,
}

impl ProofService {
    pub fn new(
        proof_engine_config: ProofEngineConfig,
        chain_config: ChainConfig,
        zkvm_enabled_cl_clients: Vec<Arc<ClClient>>,
        block_cache: Arc<Mutex<LruCache<String, ElBlockWitness>>>,
        storage: Option<Arc<Mutex<BlockStorage>>>,
        proof_rx: mpsc::Receiver<ProofServiceMessage>,
    ) -> anyhow::Result<Self> {
        let proof_engine_client = Arc::new(ProofEngineClient::new(
            proof_engine_config.url.clone(),
            proof_engine_config.proof_types.clone(),
        )?);

        let proof_cache = Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(1024).unwrap())));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let pending_proofs = Arc::new(Mutex::new(HashMap::new()));
        let proof_gen_ids = Arc::new(Mutex::new(HashMap::new()));

        Ok(Self {
            webhook_port: proof_engine_config.webhook_port,
            proof_engine_client,
            chain_config,
            zkvm_enabled_cl_clients,
            block_cache,
            proof_cache,
            storage,
            pending_requests,
            pending_proofs,
            proof_gen_ids,
            proof_rx: Arc::new(Mutex::new(proof_rx)),
        })
    }

    pub async fn run(self, shutdown_token: CancellationToken) -> anyhow::Result<()> {
        let app = Router::new()
            .route("/proofs", post(Self::proof_webhook))
            .with_state(self.clone())
            .layer(TraceLayer::new_for_http());

        let addr = format!("127.0.0.1:{}", self.webhook_port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        info!(addr = %addr, "HTTP server listening for proof pushes");

        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!(error = %e, "HTTP server error");
            }
        });

        let mut proof_rx = self.proof_rx.lock().await;
        let mut cleanup_interval = tokio::time::interval(CLEANUP_INTERVAL);

        loop {
            tokio::select! {
                biased;

                _ = shutdown_token.cancelled() => {
                    info!("ProofService received shutdown signal");
                    break;
                }

                _ = cleanup_interval.tick() => {
                    self.cleanup_stale_entries().await;
                }

                Some(message) = proof_rx.recv() => {
                    match message {
                        ProofServiceMessage::RequestProof {
                            slot,
                            block_root,
                            execution_block_hash,
                            target_clients,
                            target_proof_types,
                        } => {
                            for proof_type in target_proof_types.filter(self.proof_engine_client.proof_types().iter().cloned()) {
                                self.handle_request_proof(
                                    slot,
                                    block_root.clone(),
                                    execution_block_hash.clone(),
                                    target_clients.clone(),
                                    proof_type,
                                ).await;
                            }
                        }
                        ProofServiceMessage::BlockDataReady { block_hash } => {
                            self.handle_block_data_ready(block_hash).await;
                        }
                    }
                }

                else => break,
            }
        }

        Ok(())
    }

    async fn cleanup_stale_entries(&self) {
        let mut pending_requests_guard = self.pending_requests.lock().await;

        for (block_hash, proof_type_map) in pending_requests_guard.iter_mut() {
            proof_type_map.retain(|proof_type, pending_request| {
                let is_stale = pending_request.created_at.elapsed() >= PENDING_REQUEST_TIMEOUT;
                if is_stale {
                    warn!(
                        block_hash = %block_hash,
                        proof_type = %proof_type,
                        slot = pending_request.slot,
                        elapsed_secs = pending_request.created_at.elapsed().as_secs(),
                        "Removing stale pending request"
                    );
                }
                !is_stale
            });
        }

        pending_requests_guard.retain(|_, map| !map.is_empty());

        drop(pending_requests_guard);

        let mut pending_proofs_guard = self.pending_proofs.lock().await;
        let mut proof_gen_ids_guard = self.proof_gen_ids.lock().await;

        let mut stale_proof_keys = Vec::new();
        for (proof_key, pending_proof) in pending_proofs_guard.iter() {
            if pending_proof.created_at.elapsed() >= PENDING_REQUEST_TIMEOUT {
                warn!(
                    block_hash = %proof_key.0,
                    proof_type = %proof_key.1,
                    slot = pending_proof.slot,
                    elapsed_secs = pending_proof.created_at.elapsed().as_secs(),
                    "Removing stale pending proof"
                );
                stale_proof_keys.push((proof_key.clone(), pending_proof.proof_gen_id.clone()));
            }
        }
        for (proof_key, proof_gen_id) in stale_proof_keys {
            pending_proofs_guard.remove(&proof_key);
            proof_gen_ids_guard.remove(&proof_gen_id);
        }
    }

    async fn handle_request_proof(
        &self,
        slot: u64,
        block_root: String,
        execution_block_hash: String,
        target_clients: Target<String>,
        proof_type: ElProofType,
    ) {
        info!(
            slot = slot,
            block_root = %block_root,
            exec_hash = %execution_block_hash,
            proof_type = %proof_type,
            "Processing proof request"
        );

        if let Some(saved_proof) = self.load_proof(&execution_block_hash, proof_type).await {
            self.submit_proofs(
                &target_clients,
                slot,
                &execution_block_hash,
                &block_root,
                proof_type,
                saved_proof.proof_data,
            )
            .await;
            return;
        }

        if is_el_data_available(&self.block_cache, &self.storage, &execution_block_hash).await {
            self.request_proof(
                slot,
                &block_root,
                &execution_block_hash,
                &target_clients,
                proof_type,
            )
            .await;
        } else {
            debug!(block_hash = %execution_block_hash, proof_type = %proof_type, "EL data not ready, inserting pending proof request");
            let mut pending_guard = self.pending_requests.lock().await;
            let block_requests = pending_guard
                .entry(execution_block_hash.clone())
                .or_default();

            block_requests
                .entry(proof_type)
                .and_modify(|existing| {
                    existing.target_clients = existing.target_clients.union(&target_clients);
                })
                .or_insert_with(|| PendingRequest {
                    slot,
                    block_root: block_root.clone(),
                    target_clients: target_clients.clone(),
                    created_at: Instant::now(),
                });
        }
    }

    async fn handle_block_data_ready(&self, block_hash: String) {
        debug!(block_hash = %block_hash, "Block ready notification received");

        let pending_requests = self.pending_requests.lock().await.remove(&block_hash);
        let Some(pending_requests) = pending_requests else {
            debug!(block_hash = %block_hash, "No pending proof requests for this block");
            return;
        };

        for (proof_type, pending_request) in pending_requests {
            if let Some(saved_proof) = self.load_proof(&block_hash, proof_type).await {
                self.submit_proofs(
                    &pending_request.target_clients,
                    pending_request.slot,
                    &block_hash,
                    &pending_request.block_root,
                    proof_type,
                    saved_proof.proof_data,
                )
                .await;
            } else {
                self.request_proof(
                    pending_request.slot,
                    &pending_request.block_root,
                    &block_hash,
                    &pending_request.target_clients,
                    proof_type,
                )
                .await;
            }
        }
    }

    async fn load_proof(&self, block_hash: &str, proof_type: ElProofType) -> Option<SavedProof> {
        {
            let mut cache = self.proof_cache.lock().await;
            if let Some(proof) = cache.get(&(block_hash.to_string(), proof_type)) {
                debug!(block_hash = %block_hash, proof_type = %proof_type, "Load proof from cache");
                return Some(proof.clone());
            }
        }

        if let Some(storage) = &self.storage {
            let storage_guard = storage.lock().await;
            match storage_guard.load_proof(block_hash, proof_type) {
                Ok(Some(proof)) => {
                    debug!(block_hash = %block_hash, proof_type = %proof_type, "Load proof from disk");
                    drop(storage_guard);

                    let mut cache = self.proof_cache.lock().await;
                    cache.put((block_hash.to_string(), proof_type), proof.clone());

                    return Some(proof);
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(block_hash = %block_hash, proof_type = %proof_type, error = %e, "Failed to load proof from disk");
                }
            }
        }

        None
    }

    async fn request_proof(
        &self,
        slot: u64,
        block_root: &str,
        block_hash: &str,
        target_clients: &Target<String>,
        proof_type: ElProofType,
    ) {
        let (block, witness) = {
            let cache = self.block_cache.lock().await;
            match cache.peek(block_hash) {
                Some(cached) => (cached.block.clone(), cached.witness.clone()),
                None => {
                    warn!(
                        slot = slot,
                        block_hash = %block_hash,
                        "Block data not in cache for proof generation"
                    );
                    return;
                }
            }
        };

        let stateless_input = StatelessInput {
            block,
            witness,
            chain_config: self.chain_config.clone(),
        };
        let el_input = ElInput::new(stateless_input);

        let proof_id = proof_type.proof_id();
        let proof_key = (block_hash.to_string(), proof_type);

        let mut pending_proofs_guard = self.pending_proofs.lock().await;
        let mut proof_gen_ids_guard = self.proof_gen_ids.lock().await;

        if let Some(pending) = pending_proofs_guard.get(&proof_key) {
            if pending.created_at.elapsed() < PENDING_PROOF_TIMEOUT {
                debug!(
                    slot = slot,
                    block_hash = %block_hash,
                    proof_id = proof_id,
                    "Proof already in flight, skipping"
                );
                return;
            }
            warn!(
                slot = slot,
                block_hash = %block_hash,
                proof_id = proof_id,
                elapsed_secs = ?pending.created_at.elapsed().as_secs(),
                "Proof request timed out, retrying"
            );

            proof_gen_ids_guard.remove(&pending.proof_gen_id);
        }

        match self
            .proof_engine_client
            .request_proof(&proof_type, &el_input)
            .await
        {
            Ok(proof_gen_id) => {
                let pending_proof = PendingProof {
                    proof_type,
                    slot,
                    block_hash: block_hash.to_string(),
                    beacon_block_root: block_root.to_string(),
                    target_clients: target_clients.clone(),
                    created_at: Instant::now(),
                    proof_gen_id: proof_gen_id.clone(),
                };

                pending_proofs_guard.insert(proof_key.clone(), pending_proof);
                drop(pending_proofs_guard);

                proof_gen_ids_guard.insert(proof_gen_id.clone(), proof_key);
                drop(proof_gen_ids_guard);

                info!(
                    slot = slot,
                    block_hash = %block_hash,
                    proof_id = proof_id,
                    proof_gen_id = %proof_gen_id,
                    "Proof job submitted to proof engine"
                );
            }
            Err(e) => {
                drop(pending_proofs_guard);
                drop(proof_gen_ids_guard);

                error!(
                    slot = slot,
                    block_hash = %block_hash,
                    proof_id = proof_id,
                    error = %e,
                    "Failed to submit proof to proof engine"
                );
            }
        }
    }

    async fn proof_webhook(
        State(state): State<ProofService>,
        Json(proof_result): Json<ProofResult>,
    ) -> Result<StatusCode, (StatusCode, String)> {
        debug!(
            proof_gen_id = %proof_result.proof_gen_id,
            "Proof received from proof engine via webhook"
        );

        let Some(proof_key) = state
            .proof_gen_ids
            .lock()
            .await
            .remove(&proof_result.proof_gen_id)
        else {
            error!(
                proof_gen_id = %proof_result.proof_gen_id,
                "Unknown proof_gen_id"
            );
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Unknown proof_gen_id {}", proof_result.proof_gen_id),
            ));
        };

        let Some(pending_proof) = state.pending_proofs.lock().await.remove(&proof_key) else {
            error!(
                proof_gen_id = %proof_result.proof_gen_id,
                block_hash = %proof_key.0,
                proof_type = %proof_key.1,
                "Missing pending proof"
            );
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Missing pending proof".to_string(),
            ));
        };

        if let Some(error) = &proof_result.error {
            // TODO: Figure out the proof generatoin retry strategy.

            error!(
                proof_gen_id = %proof_result.proof_gen_id,
                proof_type = %pending_proof.proof_type,
                slot = pending_proof.slot,
                block_hash = %pending_proof.block_hash,
                error = %error,
                "Proof generation failed"
            );
            return Ok(StatusCode::OK);
        }

        let mut cache = state.proof_cache.lock().await;
        cache.put(
            (pending_proof.block_hash.clone(), pending_proof.proof_type),
            SavedProof {
                proof_type: pending_proof.proof_type,
                proof_data: proof_result.proof.clone(),
            },
        );
        drop(cache);

        if let Some(ref storage) = state.storage {
            let storage_guard = storage.lock().await;
            if let Err(e) = storage_guard.save_proof(
                &pending_proof.block_hash,
                pending_proof.proof_type,
                &proof_result.proof,
            ) {
                warn!(slot = pending_proof.slot, error = %e, "Failed to save proof to disk");
            } else {
                info!(
                    proof_gen_id = %proof_result.proof_gen_id,
                    proof_type = %pending_proof.proof_type,
                    slot = pending_proof.slot,
                    "Proof saved"
                );
            }
        }
        state
            .submit_proofs(
                &pending_proof.target_clients,
                pending_proof.slot,
                &pending_proof.block_hash,
                &pending_proof.beacon_block_root,
                pending_proof.proof_type,
                proof_result.proof.clone(),
            )
            .await;

        Ok(StatusCode::OK)
    }

    async fn submit_proofs(
        &self,
        target_clients: &Target<String>,
        slot: u64,
        block_hash: &str,
        block_root: &str,
        proof_type: ElProofType,
        proof_data: Vec<u8>,
    ) {
        let cl_clients = target_clients
            .filter_by_key(&self.zkvm_enabled_cl_clients, |cl_client| cl_client.name())
            .collect::<Vec<_>>();

        let proof_id = proof_type.proof_id();

        info!(
            slot = slot,
            block_hash = block_hash,
            cls = ?cl_clients.iter().map(|c| c.name()).collect::<Vec<_>>(),
            proof_id = proof_id,
            "Proof submitting to CL"
        );

        for client in cl_clients {
            let execution_proof = ExecutionProof {
                proof_id,
                slot,
                block_hash: block_hash.to_string(),
                block_root: block_root.to_string(),
                proof_data: proof_data.clone(),
            };

            tokio::spawn(Self::submit_proof(Arc::clone(client), execution_proof));
        }
    }

    /// Submit a proof to a single CL client with internal retry loop and exponential backoff.
    async fn submit_proof(client: Arc<ClClient>, execution_proof: ExecutionProof) {
        let cl_name = client.name().to_string();
        let slot = execution_proof.slot;
        let proof_id = execution_proof.proof_id;

        for retry_count in 0..=PROOF_SUBMISSION_MAX_RETRIES {
            match client.submit_execution_proof(&execution_proof).await {
                Ok(()) => {
                    if retry_count == 0 {
                        info!(
                            cl = %cl_name,
                            slot = slot,
                            proof_id = proof_id,
                            "Proof submitted to CL"
                        );
                    } else {
                        info!(
                            cl = %cl_name,
                            slot = slot,
                            proof_id = proof_id,
                            retry_count = retry_count,
                            "Proof submitted to CL (retry succeeded)"
                        );
                    }
                    return;
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("already known") {
                        debug!(
                            cl = %cl_name,
                            slot = slot,
                            proof_id = proof_id,
                            "Proof already known to CL"
                        );
                        return;
                    }

                    if retry_count >= PROOF_SUBMISSION_MAX_RETRIES {
                        error!(
                            cl = %cl_name,
                            slot = slot,
                            proof_id = proof_id,
                            retry_count = retry_count,
                            error = %e,
                            "Proof submission to CL failed, max retries exceeded"
                        );
                        return;
                    }

                    let backoff = Duration::from_secs(2u64.pow(retry_count));
                    warn!(
                        cl = %cl_name,
                        slot = slot,
                        proof_id = proof_id,
                        retry_count = retry_count,
                        next_retry_secs = backoff.as_secs(),
                        error = %e,
                        "Proof submission to CL failed, retrying"
                    );
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
}

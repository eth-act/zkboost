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
    service::{Target, is_el_data_ready},
    storage::SavedProof,
};

const PROOF_TIMEOUT: Duration = Duration::from_secs(300);

pub enum ProofServiceMessage {
    BlockDataReady {
        block_hash: String,
    },
    RequestProof {
        cl_name: String,
        slot: u64,
        block_root: String,
        execution_block_hash: String,
        target_clients: Target<String>,
        target_proof_types: Target<ElProofType>,
    },
}

#[derive(Clone)]
pub struct PendingProofRequest {
    slot: u64,
    block_root: String,
    target_clients: Target<String>,
}

#[derive(Debug, Clone)]
pub struct PendingProof {
    proof_type: ElProofType,
    slot: u64,
    block_hash: String,
    beacon_block_root: String,
    target_clients: Target<String>,
    start: Instant,
    proof_gen_id: ProofGenId,
}

#[derive(Clone)]
pub struct ProofService {
    webhook_port: u16,
    proof_engine_client: Arc<ProofEngineClient>,
    chain_config: ChainConfig,
    zkvm_enabled_cl_clients: Vec<Arc<ClClient>>,
    block_cache: Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    proof_cache: Arc<Mutex<LruCache<String, BTreeMap<ElProofType, SavedProof>>>>,
    storage: Option<Arc<Mutex<BlockStorage>>>,
    pending_proof_requests: Arc<Mutex<HashMap<String, BTreeMap<ElProofType, PendingProofRequest>>>>,
    pending_proofs: Arc<Mutex<HashMap<(String, ElProofType), PendingProof>>>,
    proof_gen_ids: Arc<Mutex<HashMap<ProofGenId, (String, ElProofType)>>>,
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
        let pending_proof_requests = Arc::new(Mutex::new(HashMap::new()));
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
            pending_proof_requests,
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

        loop {
            tokio::select! {
                biased;

                _ = shutdown_token.cancelled() => {
                    info!("ProofService received shutdown signal");
                    break;
                }

                Some(message) = proof_rx.recv() => {
                    match message {
                        ProofServiceMessage::RequestProof {
                            cl_name,
                            slot,
                            block_root,
                            execution_block_hash,
                            target_clients,
                            target_proof_types,
                        } => {
                            for proof_type in target_proof_types.filter(self.proof_engine_client.proof_types().iter().cloned()) {
                                self.handle_request_proof(
                                    cl_name.clone(),
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

    async fn handle_request_proof(
        &self,
        cl_name: String,
        slot: u64,
        block_root: String,
        execution_block_hash: String,
        target_clients: Target<String>,
        proof_type: ElProofType,
    ) {
        info!(
            source = %cl_name,
            slot = slot,
            block_root = %block_root,
            exec_hash = %execution_block_hash,
            proof_type = %proof_type,
            "Processing proof request"
        );

        if let Some(saved_proof) = self.load_proof(&execution_block_hash, proof_type).await {
            self.submit_proof(
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

        if is_el_data_ready(&self.block_cache, &self.storage, &execution_block_hash).await {
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
            let mut pending_guard = self.pending_proof_requests.lock().await;
            let block_requests = pending_guard
                .entry(execution_block_hash.clone())
                .or_default();

            block_requests
                .entry(proof_type)
                .and_modify(|existing| {
                    existing.target_clients = existing.target_clients.union(&target_clients);
                })
                .or_insert_with(|| PendingProofRequest {
                    slot,
                    block_root: block_root.clone(),
                    target_clients: target_clients.clone(),
                });
        }
    }

    async fn handle_block_data_ready(&self, block_hash: String) {
        debug!(block_hash = %block_hash, "Block ready notification received");

        let pending_requests = self.pending_proof_requests.lock().await.remove(&block_hash);
        let Some(pending_requests) = pending_requests else {
            debug!(block_hash = %block_hash, "No pending proof requests for this block");
            return;
        };

        if pending_requests.is_empty() {
            return;
        }

        for (proof_type, pending_request) in pending_requests {
            if let Some(saved_proof) = self.load_proof(&block_hash, proof_type).await {
                self.submit_proof(
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
            if let Some(proofs) = cache.get(block_hash)
                && let Some(proof) = proofs.get(&proof_type)
            {
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
                    cache
                        .get_or_insert_mut(block_hash.to_string(), Default::default)
                        .insert(proof_type, proof.clone());

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
            if pending.start.elapsed() < PROOF_TIMEOUT {
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
                elapsed_secs = ?pending.start.elapsed().as_secs(),
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
                    start: Instant::now(),
                    proof_gen_id: proof_gen_id.clone(),
                };

                pending_proofs_guard.insert(proof_key.clone(), pending_proof);
                proof_gen_ids_guard.insert(proof_gen_id.clone(), proof_key);

                info!(
                    slot = slot,
                    block_hash = %block_hash,
                    proof_id = proof_id,
                    proof_gen_id = %proof_gen_id,
                    "Proof job submitted to proof engine"
                );
            }
            Err(e) => {
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
            error!(
                proof_gen_id = %proof_result.proof_gen_id,
                proof_type = %pending_proof.proof_type,
                error = %error,
                "Proof generation failed"
            );

            return Ok(StatusCode::OK);
        }

        let mut cache = state.proof_cache.lock().await;
        cache
            .get_or_insert_mut(pending_proof.block_hash.clone(), Default::default)
            .insert(
                pending_proof.proof_type,
                SavedProof {
                    proof_type: pending_proof.proof_type,
                    proof_data: proof_result.proof.clone(),
                },
            );
        drop(cache);

        if let Some(ref storage) = state.storage {
            let storage_guard = storage.lock().await;
            if let Err(e) = storage_guard.save_proof(
                pending_proof.slot,
                &pending_proof.beacon_block_root,
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
            .submit_proof(
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

    async fn submit_proof(
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

        for client in &cl_clients {
            let execution_proof = ExecutionProof {
                proof_id,
                slot,
                block_hash: block_hash.to_string(),
                block_root: block_root.to_string(),
                proof_data: proof_data.clone(),
            };

            match client.submit_execution_proof(&execution_proof).await {
                Ok(()) => {
                    info!(
                        cl = %client.name(),
                        slot = slot,
                        proof_id = proof_id,
                        "Proof submitted to CL"
                    );
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("already known") {
                        debug!(
                            cl = %client.name(),
                            slot = slot,
                            proof_id = proof_id,
                            error = %e,
                            "Proof submission to CL failed"
                        );
                    }
                }
            }
        }
    }
}

use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
    config::ProofEngineConfig, service::is_el_data_ready, storage::SavedProof,
};

const PROOF_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub enum TargetClients {
    All,
    Partial(HashSet<String>),
}

impl TargetClients {
    fn union(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::Partial(lhs), Self::Partial(rhs)) => {
                Self::Partial(lhs.union(rhs).cloned().collect())
            }
        }
    }
}

impl<A: AsRef<str>> FromIterator<A> for TargetClients {
    fn from_iter<T: IntoIterator<Item = A>>(iter: T) -> Self {
        Self::Partial(iter.into_iter().map(|t| t.as_ref().to_string()).collect())
    }
}

pub enum ProofServiceMessage {
    BlockDataReady {
        block_hash: String,
    },
    RequestProof {
        cl_name: String,
        slot: u64,
        block_root: String,
        execution_block_hash: String,
        target_clients: TargetClients,
    },
}

#[derive(Clone)]
pub struct PendingProofRequest {
    pub slot: u64,
    pub block_root: String,
    pub block_hash: String,
    pub target_clients: TargetClients,
}

#[derive(Debug, Clone)]
pub struct PendingProof {
    pub proof_id: u8,
    pub proof_type: ElProofType,
    pub slot: u64,
    pub block_hash: String,
    pub beacon_block_root: String,
    pub target_clients: TargetClients,
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
    pending_proof_requests: Arc<Mutex<HashMap<String, PendingProofRequest>>>,
    pending_proofs: Arc<Mutex<HashMap<ProofGenId, PendingProof>>>,
    in_flight_proofs: Arc<Mutex<HashMap<(String, ElProofType), Instant>>>,
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
        let in_flight_proofs: Arc<Mutex<HashMap<(String, ElProofType), Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));

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
            in_flight_proofs,
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
                        } => {
                            self.handle_request_proof(
                                cl_name,
                                slot,
                                block_root,
                                execution_block_hash,
                                target_clients,
                            ).await;
                        }
                        ProofServiceMessage::BlockDataReady { block_hash } => {
                            self.handle_block_data_ready(
                                block_hash,
                            ).await;
                        }
                    }
                }

                else => break,
            }
        }

        Ok(())
    }

    async fn proof_webhook(
        State(state): State<ProofService>,
        Json(proof_result): Json<ProofResult>,
    ) -> Result<StatusCode, (StatusCode, String)> {
        debug!(
            proof_gen_id = %proof_result.proof_gen_id,
            "Proof received from proof engine via webhook"
        );

        let Some(pending_proof) = state
            .pending_proofs
            .lock()
            .await
            .remove(&proof_result.proof_gen_id)
        else {
            error!(
                proof_gen_id = %proof_result.proof_gen_id,
                "Unknown proof result"
            );
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Unknown proof result {}", proof_result.proof_gen_id),
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
            .submit_proofs(
                &pending_proof.target_clients,
                pending_proof.slot,
                &pending_proof.block_hash,
                &pending_proof.beacon_block_root,
                vec![(pending_proof.proof_id, proof_result.proof.clone())],
            )
            .await;

        let proof_key = (pending_proof.block_hash.clone(), pending_proof.proof_type);
        state.in_flight_proofs.lock().await.remove(&proof_key);

        Ok(StatusCode::OK)
    }

    async fn handle_request_proof(
        &self,
        cl_name: String,
        slot: u64,
        block_root: String,
        execution_block_hash: String,
        target_clients: TargetClients,
    ) {
        info!(
            source = %cl_name,
            slot = slot,
            block_root = %block_root,
            exec_hash = %execution_block_hash,
            "Processing proof request"
        );

        let proofs = self.load_proofs(&execution_block_hash).await;

        if let Some(proofs) = proofs {
            let proof_list: Vec<(u8, Vec<u8>)> = proofs
                .values()
                .map(|p| (p.proof_type.proof_id(), p.proof_data.clone()))
                .collect();
            self.submit_proofs(
                &target_clients,
                slot,
                &execution_block_hash,
                &block_root,
                proof_list,
            )
            .await;
            return;
        }

        if is_el_data_ready(&self.block_cache, &self.storage, &execution_block_hash).await {
            self.request_proofs(slot, &block_root, &execution_block_hash, &target_clients)
                .await;
        } else {
            debug!(block_hash = %execution_block_hash, "EL data not ready, inserting pending proof request");
            self.pending_proof_requests
                .lock()
                .await
                .entry(execution_block_hash.clone())
                .and_modify(|request| {
                    request.target_clients = request.target_clients.union(&target_clients);
                })
                .or_insert_with(|| PendingProofRequest {
                    slot,
                    block_root,
                    block_hash: execution_block_hash,
                    target_clients,
                });
        }
    }

    async fn handle_block_data_ready(&self, block_hash: String) {
        debug!(block_hash = %block_hash, "Block ready notification received");

        let pending_proof = self.pending_proof_requests.lock().await.remove(&block_hash);
        if pending_proof.is_none() {
            debug!(block_hash = %block_hash, "No pending proof requests for this block");
            return;
        }

        let pending_proof = pending_proof.unwrap();

        let proofs = self.load_proofs(&pending_proof.block_hash).await;

        if let Some(proofs) = proofs {
            let proof_list: Vec<(u8, Vec<u8>)> = proofs
                .values()
                .map(|p| (p.proof_type.proof_id(), p.proof_data.clone()))
                .collect();
            self.submit_proofs(
                &pending_proof.target_clients,
                pending_proof.slot,
                &pending_proof.block_hash,
                &pending_proof.block_root,
                proof_list,
            )
            .await;
        } else {
            self.request_proofs(
                pending_proof.slot,
                &pending_proof.block_root,
                &pending_proof.block_hash,
                &pending_proof.target_clients,
            )
            .await;
        }
    }

    async fn load_proofs(&self, block_hash: &str) -> Option<BTreeMap<ElProofType, SavedProof>> {
        {
            let mut cache = self.proof_cache.lock().await;
            if let Some(proofs) = cache.get(block_hash) {
                debug!(block_hash = %block_hash, num_proofs = proofs.len(), "Load proofs from cache");
                return Some(proofs.clone());
            }
        }

        if let Some(storage) = &self.storage {
            let storage_guard = storage.lock().await;
            match storage_guard.load_proofs(block_hash) {
                Ok(Some((metadata, proofs))) if !proofs.is_empty() => {
                    debug!(
                        block_hash = %block_hash,
                        num_proofs = proofs.len(),
                        "Load proofs from disk"
                    );
                    drop(storage_guard);

                    let proof_map: BTreeMap<ElProofType, SavedProof> = proofs
                        .into_iter()
                        .map(|proof| (proof.proof_type, proof))
                        .collect();

                    let mut cache = self.proof_cache.lock().await;
                    cache.put(block_hash.to_string(), proof_map.clone());

                    return Some(proof_map);
                }
                Ok(_) => {
                    debug!(block_hash = %block_hash, "No saved proofs found");
                }
                Err(e) => {
                    warn!(block_hash = %block_hash, error = %e, "Failed to load proofs from disk");
                }
            }
        }

        None
    }

    async fn submit_proofs(
        &self,
        target_clients: &TargetClients,
        slot: u64,
        block_hash: &str,
        block_root: &str,
        proofs: Vec<(u8, Vec<u8>)>,
    ) {
        let cl_clients = match target_clients {
            TargetClients::All => self.zkvm_enabled_cl_clients.clone(),
            TargetClients::Partial(partial) => self
                .zkvm_enabled_cl_clients
                .iter()
                .filter(|client| partial.contains(&client.name().to_string()))
                .cloned()
                .collect(),
        };

        info!(
            slot = slot,
            block_hash = block_hash,
            cls = ?cl_clients.iter().map(|c| c.name()).collect::<Vec<_>>(),
            proof_ids = ?proofs.iter().map(|(id,_)| *id).collect::<Vec<_>>(),
            "Proof submitting to CL"
        );

        for (proof_id, proof_data) in proofs {
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

    async fn request_proofs(
        &self,
        slot: u64,
        block_root: &str,
        block_hash: &str,
        target_clients: &TargetClients,
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

        let el_input = ElInput::new(stateless_input.clone());

        for &proof_type in self.proof_engine_client.proof_types() {
            let proof_id = proof_type.proof_id();
            let proof_key = (block_hash.to_string(), proof_type);

            let mut in_flight_guard = self.in_flight_proofs.lock().await;
            if let Some(started_at) = in_flight_guard.get(&proof_key) {
                if started_at.elapsed() < PROOF_TIMEOUT {
                    debug!(
                        slot = slot,
                        block_hash = %block_hash,
                        proof_id = proof_id,
                        "Proof already in flight, skipping"
                    );
                    continue;
                }
                warn!(
                    slot = slot,
                    block_hash = %block_hash,
                    proof_id = proof_id,
                    elapsed_secs = ?started_at.elapsed().as_secs(),
                    "Proof request timed out, retrying"
                );
            }

            let mut pending_proofs_guard = self.pending_proofs.lock().await;

            match self
                .proof_engine_client
                .request_proof(&proof_type, &el_input)
                .await
            {
                Ok(proof_gen_id) => {
                    let pending_proof = PendingProof {
                        proof_id,
                        proof_type,
                        slot,
                        block_hash: block_hash.to_string(),
                        beacon_block_root: block_root.to_string(),
                        target_clients: target_clients.clone(),
                    };

                    pending_proofs_guard.insert(proof_gen_id.clone(), pending_proof);
                    in_flight_guard.insert(proof_key, Instant::now());

                    drop(pending_proofs_guard);
                    drop(in_flight_guard);

                    info!(
                        slot = slot,
                        proof_id = proof_id,
                        proof_gen_id = %proof_gen_id,
                        "Proof job submitted to proof engine"
                    );
                }
                Err(e) => {
                    drop(pending_proofs_guard);
                    drop(in_flight_guard);

                    error!(
                        slot = slot,
                        proof_id = proof_id,
                        error = %e,
                        "Failed to submit proof to proof engine"
                    );
                }
            }
        }
    }
}

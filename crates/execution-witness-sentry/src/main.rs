//! Relayer for execution proof.
//!
//! This relayer orchestrates the complete workflow for generating proofs of
//! execution proof:
//!
//! 1. Listen to new block from CL
//! 2. Fetch execution witness from EL
//! 3. Generate input for EL stateless validator guest program
//! 4. Request Proof Engine (zkboost) for proof
//! 5. Send proof back to CL
//!
//! ## Architecture
//!
//! ```text
//!   CL          Relayer               EL            Proof Engine
//!   |              |                  |                  |
//!   |--new block-->|                  |                  |
//!   |              |                  |                  |
//!   |              |--fetch witness-->|                  |
//!   |              |<----witness------|                  |
//!   |              |                  |                  |
//!   |    (generate zkVM input)        |                  |
//!   |              |                  |                  |
//!   |              |--request proof--------------------->|
//!   |              |                  |                  |
//!   |              |                  |           (generate proof)
//!   |              |                  |                  |
//!   |              |<------proof-------------------------|
//!   |              |                  |                  |
//!   |<----proof----|                  |                  |
//!   |              |                  |                  |
//! ```

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    num::NonZeroUsize,
    path::PathBuf,
    pin::pin,
    sync::Arc,
    time::Duration,
};

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use clap::Parser;
use execution_witness_sentry::{
    BlockStorage, ClClient, ClEvent, Config, ElBlockWitness, ElClient, ExecutionProof,
    ProofEngineClient, storage::SavedProof, subscribe_blocks, subscribe_cl_events,
};
use futures::StreamExt;
use lru::LruCache;
use reth_chainspec::mainnet_chain_config;
use reth_stateless::StatelessInput;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};
use url::Url;
use zkboost_ethereum_el_input::ElInput;
use zkboost_ethereum_el_types::ElProofType;
use zkboost_types::{ProofGenId, ProofResult};

#[derive(Parser, Debug)]
#[command(name = "execution-witness-sentry")]
#[command(about = "Monitor execution layer nodes and fetch execution witnesses")]
struct Cli {
    #[arg(long, short, default_value = "config.toml")]
    config: PathBuf,
}

#[derive(Clone)]
struct ClBlockEvent {
    cl_name: String,
    slot: u64,
    block_root: String,
    execution_block_hash: String,
}

#[derive(Debug, Clone)]
struct ZkvmClStatus {
    name: String,
    head_slot: u64,
    gap: i64,
}

#[derive(Clone)]
struct BlockFetchRequest {
    block_hash: String,
    el_endpoints: Vec<execution_witness_sentry::ElEndpoint>,
}

struct BlockReadyNotification {
    block_hash: String,
}

/// Proof request.
#[derive(Clone)]
struct ProofRequest {
    cl_block_event: ClBlockEvent,
    /// CL clients to submit proof to when proof is ready.
    target_clients: Option<HashSet<String>>,
}

/// Proof request pending for EL block data.
#[derive(Clone)]
struct PendingProofRequest {
    slot: u64,
    block_root: String,
    block_hash: String,
    /// CL clients to submit proof to when proof is ready.
    target_clients: Option<HashSet<String>>,
}

/// Pending proof from proof engine.
#[derive(Debug, Clone)]
struct PendingProof {
    proof_id: u8,
    proof_type: ElProofType,
    slot: u64,
    block_hash: String,
    beacon_block_root: String,
    /// CL clients to submit proof to when proof is ready.
    target_clients: Option<HashSet<String>>,
}

type PendingProofs = Arc<Mutex<HashMap<ProofGenId, PendingProof>>>;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    block_cache: Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    proof_cache: Arc<Mutex<LruCache<String, BTreeMap<ElProofType, SavedProof>>>>,
    storage: Option<Arc<Mutex<BlockStorage>>>,
    pending_proofs: PendingProofs,
    in_flight_proofs: Arc<Mutex<HashSet<(String, u8)>>>,
    proof_engine_client: Arc<ProofEngineClient>,
    zkvm_enabled_clients: Vec<Arc<ClClient>>,
}

async fn proof_webhook(
    State(state): State<AppState>,
    Json(proof_result): Json<ProofResult>,
) -> Result<StatusCode, (StatusCode, String)> {
    info!(
        proof_gen_id = %proof_result.proof_gen_id,
        "Received proof push from proof engine-server"
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

    info!(
        proof_gen_id = %proof_result.proof_gen_id,
        proof_type = %pending_proof.proof_type,
        slot = pending_proof.slot,
        "Proof completed"
    );

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

    if let Some(storage) = state.storage {
        let storage_guard = storage.lock().await;
        if let Err(e) = storage_guard.save_proof(
            pending_proof.slot,
            &pending_proof.beacon_block_root,
            &pending_proof.block_hash,
            pending_proof.proof_type,
            &proof_result.proof,
        ) {
            warn!(slot = pending_proof.slot, error = %e, "Failed to save proof to disk");
        }
    }

    let cl_clients = filter_target_clients(
        &state.zkvm_enabled_clients,
        pending_proof.target_clients.as_ref(),
    );
    submit_proofs(
        &cl_clients,
        pending_proof.slot,
        &pending_proof.block_hash,
        &pending_proof.beacon_block_root,
        vec![(pending_proof.proof_id, proof_result.proof.clone())],
    )
    .await;

    let proof_key = (pending_proof.block_hash.clone(), pending_proof.proof_id);
    state.in_flight_proofs.lock().await.remove(&proof_key);

    Ok(StatusCode::OK)
}

fn filter_target_clients(
    zkvm_enabled_clients: &[Arc<ClClient>],
    target_clients: Option<&HashSet<String>>,
) -> Vec<Arc<ClClient>> {
    if let Some(target_clients) = target_clients {
        zkvm_enabled_clients
            .iter()
            .filter(|client| target_clients.contains(&client.name().to_string()))
            .cloned()
            .collect()
    } else {
        zkvm_enabled_clients.to_vec()
    }
}

async fn monitor_zkvm_status(
    source_client: &ClClient,
    zkvm_enabled_clients: &[Arc<ClClient>],
) -> Vec<ZkvmClStatus> {
    let source_head = match source_client.get_head_slot().await {
        Ok(slot) => slot,
        Err(e) => {
            warn!(error = %e, "Failed to get source CL head");
            return vec![];
        }
    };

    let mut statuses = Vec::new();
    for client in zkvm_enabled_clients {
        match client.get_head_slot().await {
            Ok(head_slot) => {
                let gap = head_slot as i64 - source_head as i64;
                statuses.push(ZkvmClStatus {
                    name: client.name().to_string(),
                    head_slot,
                    gap,
                });
            }
            Err(e) => {
                warn!(name = %client.name(), error = %e, "Failed to get zkvm CL head");
            }
        }
    }

    statuses
}

async fn block_fetcher_task(
    mut block_fetch_request_rx: tokio::sync::mpsc::Receiver<BlockFetchRequest>,
    block_ready_tx: tokio::sync::mpsc::Sender<BlockReadyNotification>,
    block_cache: Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    storage: Option<Arc<Mutex<BlockStorage>>>,
) {
    let in_flight = Arc::new(Mutex::new(HashSet::new()));

    while let Some(request) = block_fetch_request_rx.recv().await {
        {
            let in_flight_guard = in_flight.lock().await;
            if in_flight_guard.contains(&request.block_hash) {
                debug!(block_hash = %request.block_hash, "Block fetch already in flight, skipping");
                continue;
            }
        }

        {
            let cache = block_cache.lock().await;
            if cache.contains(&request.block_hash) {
                debug!(block_hash = %request.block_hash, "Block already in cache");
                if let Err(e) = block_ready_tx
                    .send(BlockReadyNotification {
                        block_hash: request.block_hash.clone(),
                    })
                    .await
                {
                    error!(error = %e, "Failed to send block ready notification");
                }
                continue;
            }
        }

        if let Some(ref storage) = storage {
            let storage_guard = storage.lock().await;
            match storage_guard.load_block_and_witness(&request.block_hash) {
                Ok(Some((block, witness))) => {
                    drop(storage_guard);

                    let mut cache = block_cache.lock().await;
                    cache.put(
                        request.block_hash.clone(),
                        ElBlockWitness {
                            block: block.clone(),
                            witness: witness.clone(),
                        },
                    );
                    drop(cache);

                    debug!(block_hash = %request.block_hash, "Loaded block from disk to cache");

                    if let Err(e) = block_ready_tx
                        .send(BlockReadyNotification {
                            block_hash: request.block_hash.clone(),
                        })
                        .await
                    {
                        error!(error = %e, "Failed to send block ready notification");
                    }
                    continue;
                }
                Ok(None) => {
                    debug!(block_hash = %request.block_hash, "Block not found on disk");
                }
                Err(e) => {
                    warn!(block_hash = %request.block_hash, error = %e, "Failed to load block from disk");
                }
            }
        }

        {
            let mut in_flight_guard = in_flight.lock().await;
            in_flight_guard.insert(request.block_hash.clone());
        }
        let in_flight_clone = in_flight.clone();
        let block_cache_clone = block_cache.clone();
        let storage_clone = storage.clone();
        let block_ready_tx_clone = block_ready_tx.clone();

        tokio::spawn(async move {
            let result = fetch_block_from_el(
                &request.block_hash,
                &request.el_endpoints,
                &block_cache_clone,
                &storage_clone,
            )
            .await;

            {
                let mut in_flight_guard = in_flight_clone.lock().await;
                in_flight_guard.remove(&request.block_hash);
            }

            if result.is_ok()
                && let Err(e) = block_ready_tx_clone
                    .send(BlockReadyNotification {
                        block_hash: request.block_hash.clone(),
                    })
                    .await
            {
                error!(error = %e, "Failed to send block ready notification");
            }
        });
    }
}

async fn fetch_block_from_el(
    block_hash: &str,
    el_endpoints: &[execution_witness_sentry::ElEndpoint],
    block_cache: &Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    storage: &Option<Arc<Mutex<BlockStorage>>>,
) -> anyhow::Result<()> {
    for endpoint in el_endpoints {
        let url = match Url::parse(&endpoint.url) {
            Ok(u) => u,
            Err(e) => {
                warn!(name = %endpoint.name, error = %e, "Invalid EL endpoint URL");
                continue;
            }
        };
        let el_client = ElClient::new(endpoint.name.clone(), url);

        let block = match el_client.get_block_by_hash(block_hash).await {
            Ok(Some(data)) => data,
            Ok(None) => {
                debug!(block_hash = %block_hash, "Block not found on EL");
                continue;
            }
            Err(e) => {
                warn!(block_hash = %block_hash, error = %e, "Failed to fetch block from EL");
                continue;
            }
        };

        let witness = match el_client.get_execution_witness_by_hash(block_hash).await {
            Ok(Some(data)) => data,
            Ok(None) => {
                debug!(block_hash = %block_hash, "Witness not found on EL");
                continue;
            }
            Err(e) => {
                warn!(block_hash = %block_hash, error = %e, "Failed to fetch witness from EL");
                continue;
            }
        };

        let block_number = block.header.number;
        let el_data = ElBlockWitness { block, witness };

        info!(
            block_number = block_number,
            block_hash = %block_hash,
            "Fetched block and witness from EL"
        );

        if let Some(storage) = storage {
            let mut storage_guard = storage.lock().await;
            if let Err(e) = storage_guard.save_block(&el_data) {
                warn!(block_hash = %block_hash, error = %e, "Failed to save fetched block to disk");
            } else {
                debug!(
                    block_number = block_number,
                    block_hash = %block_hash,
                    "Saved fetched block to disk"
                );
            }
        }

        let mut cache = block_cache.lock().await;
        cache.put(block_hash.to_string(), el_data);
        debug!(block_hash = %block_hash, "Cached fetched block in memory");

        return Ok(());
    }

    Err(anyhow::anyhow!(
        "Failed to fetch block from any EL endpoint"
    ))
}

async fn is_el_data_ready(state: &AppState, block_hash: &str) -> bool {
    {
        let cache = state.block_cache.lock().await;
        if cache.contains(block_hash) {
            return true;
        }
    }

    if let Some(storage) = &state.storage {
        let storage_guard = storage.lock().await;
        match storage_guard.load_block_and_witness(block_hash) {
            Ok(Some((block, witness))) => {
                drop(storage_guard);

                let mut cache = state.block_cache.lock().await;
                cache.put(
                    block_hash.to_string(),
                    ElBlockWitness {
                        block: block.clone(),
                        witness: witness.clone(),
                    },
                );

                debug!(block_hash = %block_hash, "Loaded EL data from disk to cache");
                return true;
            }
            Ok(None) => {
                debug!(block_hash = %block_hash, "EL data not found on disk");
            }
            Err(e) => {
                warn!(block_hash = %block_hash, error = %e, "Failed to load EL data from disk");
            }
        }
    }

    false
}

async fn proof_submission_task(
    mut proof_request_rx: tokio::sync::mpsc::Receiver<ProofRequest>,
    mut block_ready_rx: tokio::sync::mpsc::Receiver<BlockReadyNotification>,
    state: AppState,
) {
    let mut pending_proof_request: HashMap<String, Vec<PendingProofRequest>> = HashMap::new();

    loop {
        tokio::select! {
            Some(proof_request) = proof_request_rx.recv() => {
                handle_proof_request(
                    proof_request,
                    &mut pending_proof_request,
                    &state,
                ).await;
            }

            Some(block_ready) = block_ready_rx.recv() => {
                handle_block_ready(
                    block_ready,
                    &mut pending_proof_request,
                    &state,
                ).await;
            }

            else => break,
        }
    }
}

async fn handle_proof_request(
    proof_request: ProofRequest,
    pending_proof_request: &mut HashMap<String, Vec<PendingProofRequest>>,
    state: &AppState,
) {
    let cl_block_event = &proof_request.cl_block_event;
    let target_clients = &proof_request.target_clients;

    info!(
        source = %cl_block_event.cl_name,
        slot = cl_block_event.slot,
        block_root = %cl_block_event.block_root,
        exec_hash = %cl_block_event.execution_block_hash,
        "Processing proof request"
    );

    let proofs = load_proofs(
        &cl_block_event.execution_block_hash,
        &state.proof_cache,
        &state.storage,
    )
    .await;

    if let Some(proofs) = proofs {
        let cl_clients =
            filter_target_clients(&state.zkvm_enabled_clients, target_clients.as_ref());
        let proof_list: Vec<(u8, Vec<u8>)> = proofs
            .values()
            .map(|p| (p.proof_type.proof_id(), p.proof_data.clone()))
            .collect();
        submit_proofs(
            &cl_clients,
            cl_block_event.slot,
            &cl_block_event.execution_block_hash,
            &cl_block_event.block_root,
            proof_list,
        )
        .await;
        return;
    }

    if is_el_data_ready(state, &cl_block_event.execution_block_hash).await {
        request_proofs(
            cl_block_event.slot,
            &cl_block_event.block_root,
            &cl_block_event.execution_block_hash,
            target_clients,
            state,
        )
        .await;
    } else {
        debug!(block_hash = %cl_block_event.execution_block_hash, "EL data not ready, inserting pending proof request");
        pending_proof_request
            .entry(cl_block_event.execution_block_hash.clone())
            .or_default()
            .push(PendingProofRequest {
                slot: cl_block_event.slot,
                block_root: cl_block_event.block_root.clone(),
                block_hash: cl_block_event.execution_block_hash.clone(),
                target_clients: target_clients.clone(),
            });
    }
}

async fn handle_block_ready(
    block_ready: BlockReadyNotification,
    pending_proof_request: &mut HashMap<String, Vec<PendingProofRequest>>,
    state: &AppState,
) {
    debug!(block_hash = %block_ready.block_hash, "Block ready notification received");

    let tasks = pending_proof_request.remove(&block_ready.block_hash);
    if tasks.is_none() {
        debug!(block_hash = %block_ready.block_hash, "No pending proof tasks for this block");
        return;
    }

    let tasks = tasks.unwrap();

    for task in tasks {
        let proofs = load_proofs(&task.block_hash, &state.proof_cache, &state.storage).await;

        if let Some(proofs) = proofs {
            let cl_clients =
                filter_target_clients(&state.zkvm_enabled_clients, task.target_clients.as_ref());
            let proof_list: Vec<(u8, Vec<u8>)> = proofs
                .values()
                .map(|p| (p.proof_type.proof_id(), p.proof_data.clone()))
                .collect();
            submit_proofs(
                &cl_clients,
                task.slot,
                &task.block_hash,
                &task.block_root,
                proof_list,
            )
            .await;
            continue;
        }

        request_proofs(
            task.slot,
            &task.block_root,
            &task.block_hash,
            &task.target_clients,
            state,
        )
        .await;
    }
}

async fn load_proofs(
    block_hash: &str,
    proof_cache: &Arc<Mutex<LruCache<String, BTreeMap<ElProofType, SavedProof>>>>,
    storage: &Option<Arc<Mutex<BlockStorage>>>,
) -> Option<BTreeMap<ElProofType, SavedProof>> {
    {
        let mut cache = proof_cache.lock().await;
        if let Some(proofs) = cache.get(block_hash) {
            debug!(block_hash = %block_hash, num_proofs = proofs.len(), "Load proofs from cache");
            return Some(proofs.clone());
        }
    }

    if let Some(storage) = storage {
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

                let mut cache = proof_cache.lock().await;
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
    cl_clients: &[Arc<ClClient>],
    slot: u64,
    block_hash: &str,
    block_root: &str,
    proofs: Vec<(u8, Vec<u8>)>,
) {
    info!(
        slot = slot,
        block_hash = block_hash,
        cls = ?cl_clients.iter().map(|c| c.name()).collect::<Vec<_>>(),
        proof_ids = ?proofs.iter().map(|(id,_)| *id).collect::<Vec<_>>(),
        "Proof submitting"
    );

    for (proof_id, proof_data) in proofs {
        for client in cl_clients {
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
                        "Proof submitted"
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
                            "Proof submission failed"
                        );
                    }
                }
            }
        }
    }
}

async fn request_proofs(
    slot: u64,
    block_root: &str,
    block_hash: &str,
    target_clients: &Option<HashSet<String>>,
    state: &AppState,
) {
    let (block, witness) = {
        let cache = state.block_cache.lock().await;
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
        chain_config: mainnet_chain_config(),
    };

    let el_input = ElInput::new(stateless_input.clone());

    for &proof_type in state.proof_engine_client.proof_types() {
        let proof_id = proof_type.proof_id();
        let proof_key = (block_hash.to_string(), proof_id);

        let mut in_flight_guard = state.in_flight_proofs.lock().await;
        if in_flight_guard.contains(&proof_key) {
            debug!(
                slot = slot,
                block_hash = %block_hash,
                proof_id = proof_id,
                "Proof already in flight, skipping"
            );
            continue;
        }

        let mut pending_proofs_guard = state.pending_proofs.lock().await;

        match state
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
                in_flight_guard.insert(proof_key);

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

async fn backfill_proofs(
    source_client: &ClClient,
    zkvm_enabled_client: &ClClient,
    max_slots: u64,
    proof_request_tx: &tokio::sync::mpsc::Sender<ProofRequest>,
    block_fetch_request_tx: &tokio::sync::mpsc::Sender<BlockFetchRequest>,
    state: &AppState,
) {
    let zkvm_head = match zkvm_enabled_client.get_head_slot().await {
        Ok(slot) => slot,
        Err(e) => {
            warn!(name = %zkvm_enabled_client.name(), error = %e, "Failed to get zkvm CL head for backfill");
            return;
        }
    };

    let source_head = match source_client.get_head_slot().await {
        Ok(slot) => slot,
        Err(e) => {
            warn!(error = %e, "Failed to get source CL head for backfill");
            return;
        }
    };

    if zkvm_head >= source_head {
        return;
    }

    let gap = source_head - zkvm_head;
    let slots_to_check = gap.min(max_slots);

    info!(
        name = %zkvm_enabled_client.name(),
        zkvm_head = zkvm_head,
        source_head = source_head,
        gap = gap,
        checking = slots_to_check,
        "Backfilling proofs"
    );

    for slot in (zkvm_head + 1)..=(zkvm_head + slots_to_check) {
        let block_info = match source_client.get_block_info(slot).await {
            Ok(Some(info)) => info,
            Ok(None) => {
                debug!(slot = slot, "Empty slot, skipping");
                continue;
            }
            Err(e) => {
                debug!(slot = slot, error = %e, "Failed to get block info");
                continue;
            }
        };

        let Some(block_hash) = block_info.execution_block_hash else {
            debug!(slot = slot, "No execution payload, skipping");
            continue;
        };

        let target_clients = Some(HashSet::from_iter([zkvm_enabled_client.name().to_string()]));

        if !is_el_data_ready(state, &block_hash).await {
            debug!(slot = slot, block_hash = %block_hash, "EL data not ready for backfill, sending fetch and proof request");

            if let Err(e) = block_fetch_request_tx
                .send(BlockFetchRequest {
                    block_hash: block_hash.clone(),
                    el_endpoints: state.config.el_endpoints.clone(),
                })
                .await
            {
                error!(error = %e, "Failed to send block fetch request for backfill");
            }
        }

        let cl_block_event = ClBlockEvent {
            cl_name: source_client.name().to_string(),
            slot,
            block_root: block_info.block_root.clone(),
            execution_block_hash: block_hash.clone(),
        };
        let proof_request = ProofRequest {
            cl_block_event,
            target_clients,
        };

        if proof_request_tx.send(proof_request).await.is_err() {
            error!("Failed to send backfill proof request");
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let config = Arc::new(Config::load(&cli.config)?);

    info!(
        el_endpoints = config.el_endpoints.len(),
        "Loaded configuration"
    );
    for endpoint in &config.el_endpoints {
        info!(
            name = %endpoint.name,
            url = %endpoint.url,
            ws_url = %endpoint.ws_url,
            "EL endpoint configured"
        );
    }

    let proof_engine_client = Arc::new(ProofEngineClient::new(
        config.proof_engine.url.clone(),
        config.proof_engine.proof_types.clone(),
    )?);

    let pending_proofs: PendingProofs = Arc::new(Mutex::new(HashMap::new()));

    let block_cache = Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(128).unwrap())));

    let proof_cache = Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(128).unwrap())));

    let in_flight_proofs: Arc<Mutex<HashSet<(String, u8)>>> = Arc::new(Mutex::new(HashSet::new()));

    let mut zkvm_enabled_clients: Vec<Arc<ClClient>> = Vec::new();
    let mut event_source_client: Option<ClClient> = None;

    for endpoint in &config.cl_endpoints {
        let url = match Url::parse(&endpoint.url) {
            Ok(u) => u,
            Err(e) => {
                warn!(name = %endpoint.name, error = %e, "Invalid CL endpoint URL");
                continue;
            }
        };
        let client = ClClient::new(endpoint.name.clone(), url);

        match client.is_zkvm_enabled().await {
            Ok(true) => {
                info!(name = %endpoint.name, "CL endpoint has zkvm enabled (proof target)");
                zkvm_enabled_clients.push(Arc::new(client));
            }
            Ok(false) => {
                info!(name = %endpoint.name, "CL endpoint does not have zkvm enabled");
                if event_source_client.is_none() {
                    info!(name = %endpoint.name, "Using as event source");
                    event_source_client = Some(client);
                }
            }
            Err(e) => {
                warn!(name = %endpoint.name, error = %e, "Failed to check zkvm status");
            }
        }
    }

    info!(
        zkvm_enabled_clients = zkvm_enabled_clients.len(),
        "zkvm-enabled CL endpoints configured"
    );

    let Some(event_source) = event_source_client else {
        error!("No non-zkvm CL endpoint available for event source");
        return Ok(());
    };
    info!(name = %event_source.name(), "CL event source configured");

    let storage: Option<Arc<Mutex<BlockStorage>>> = config.output_dir.as_ref().map(|dir| {
        Arc::new(Mutex::new(BlockStorage::new(
            dir,
            config.chain.as_deref().unwrap_or("unknown"),
            config.retain,
        )))
    });

    let state = AppState {
        config,
        block_cache,
        proof_cache,
        storage,
        pending_proofs,
        in_flight_proofs,
        proof_engine_client,
        zkvm_enabled_clients,
    };

    {
        let app = Router::new()
            .route("/proofs", post(proof_webhook))
            .with_state(state.clone())
            .layer(TraceLayer::new_for_http());

        let addr = format!("127.0.0.1:{}", state.config.proof_engine.webhook_port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        info!(addr = %addr, "HTTP server listening for proof pushes");

        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!(error = %e, "HTTP server error");
            }
        });
    }

    let (proof_request_tx, proof_request_rx) = tokio::sync::mpsc::channel::<ProofRequest>(1024);
    let (block_fetch_request_tx, block_fetch_request_rx) =
        tokio::sync::mpsc::channel::<BlockFetchRequest>(1024);
    let (block_ready_tx, block_ready_rx) =
        tokio::sync::mpsc::channel::<BlockReadyNotification>(1024);

    {
        let block_cache_clone = state.block_cache.clone();
        let storage_clone = state.storage.clone();
        let block_ready_tx_clone = block_ready_tx.clone();

        tokio::spawn(async move {
            block_fetcher_task(
                block_fetch_request_rx,
                block_ready_tx_clone,
                block_cache_clone,
                storage_clone,
            )
            .await;
        });
    }

    {
        let state = state.clone();

        tokio::spawn(async move {
            proof_submission_task(proof_request_rx, block_ready_rx, state).await;
        });
    }

    for endpoint in state.config.el_endpoints.clone() {
        let tx = block_fetch_request_tx.clone();
        let name = endpoint.name.clone();
        let ws_url = endpoint.ws_url.clone();
        let endpoints = vec![endpoint.clone()];

        tokio::spawn(async move {
            info!(name = %name, "Connecting to EL WebSocket");

            let stream = match subscribe_blocks(&ws_url).await {
                Ok(s) => s,
                Err(e) => {
                    error!(name = %name, error = %e, "Failed to subscribe to EL");
                    return;
                }
            };

            info!(name = %name, "Subscribed to EL newHeads");
            let mut stream = pin!(stream);

            while let Some(result) = stream.next().await {
                match result {
                    Ok(header) => {
                        let block_hash = header.hash.to_string();
                        info!(
                            name = %name,
                            number = header.number,
                            hash = %block_hash,
                            "EL block received"
                        );

                        let req = BlockFetchRequest {
                            block_hash: block_hash.clone(),
                            el_endpoints: endpoints.clone(),
                        };
                        if let Err(error) = tx.send(req).await {
                            error!(block_hash = %block_hash, error = %error, "Failed to send block fetch request");
                        }
                    }
                    Err(e) => {
                        error!(name = %name, error = %e, "EL stream error");
                    }
                }
            }
            warn!(name = %name, "EL WebSocket stream ended");
        });
    }

    let es_client = event_source;
    let source_client_for_monitor = es_client.clone();

    {
        let tx = proof_request_tx.clone();

        tokio::spawn(async move {
            info!(name = %es_client.name(), "Connecting to CL SSE");

            let stream = match subscribe_cl_events(es_client.url()) {
                Ok(s) => s,
                Err(e) => {
                    error!(name = %es_client.name(), error = %e, "Failed to subscribe to CL events");
                    return;
                }
            };

            info!(name = %es_client.name(), "Subscribed to CL head events");
            let mut stream = pin!(stream);

            while let Some(result) = stream.next().await {
                match result {
                    Ok(ClEvent::Head(head)) => {
                        debug!(slot = %head.slot, "Received ClEvent");

                        let slot: u64 = match head.slot.parse() {
                            Ok(slot) => slot,
                            Err(e) => {
                                warn!(
                                    name = %es_client.name(),
                                    error = %e,
                                    slot = %head.slot,
                                    "Invalid head slot value"
                                );
                                continue;
                            }
                        };
                        let block_root = head.block.clone();

                        let execution_block_hash = match es_client
                            .get_block_execution_hash(&block_root)
                            .await
                        {
                            Ok(Some(hash)) => hash,
                            Ok(None) => {
                                debug!(name = %es_client.name(), slot = slot, "No execution hash for block");
                                continue;
                            }
                            Err(e) => {
                                debug!(name = %es_client.name(), error = %e, "Failed to get execution hash");
                                continue;
                            }
                        };

                        let cl_block_event = ClBlockEvent {
                            cl_name: es_client.name().to_string(),
                            slot,
                            block_root,
                            execution_block_hash,
                        };
                        let proof_request = ProofRequest {
                            cl_block_event,
                            target_clients: None,
                        };
                        if let Err(error) = tx.send(proof_request).await {
                            error!(slot = %slot, error = %error, "Failed to send proof request");
                        }
                    }
                    Ok(ClEvent::Block(_)) => {}
                    Err(e) => {
                        error!(name = %es_client.name(), error = %e, "CL stream error");
                    }
                }
            }
            warn!(name = %es_client.name(), "CL SSE stream ended");
        });
    }

    drop(block_ready_tx);

    let mut monitor_interval = tokio::time::interval(Duration::from_millis(6000));
    monitor_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!("Waiting for events with monitoring every second");

    loop {
        tokio::select! {
            _ = monitor_interval.tick() => {
                let statuses =
                    monitor_zkvm_status(&source_client_for_monitor, &state.zkvm_enabled_clients).await;

                for status in &statuses {
                    if status.gap < -5 {
                        warn!(
                            name = %status.name,
                            head_slot = status.head_slot,
                            gap = status.gap,
                            "zkvm CL is behind, starting backfill"
                        );

                        if let Some(client) = state
                            .zkvm_enabled_clients
                            .iter()
                            .find(|client| client.name() == status.name)
                        {
                            backfill_proofs(
                                &source_client_for_monitor,
                                client,
                                20,
                                &proof_request_tx,
                                &block_fetch_request_tx,
                                &state,
                            )
                            .await;
                        }
                    } else if status.gap < 0 {
                        debug!(
                            name = %status.name,
                            head_slot = status.head_slot,
                            gap = status.gap,
                            "zkvm CL slightly behind"
                        );
                    } else {
                        debug!(
                            name = %status.name,
                            head_slot = status.head_slot,
                            gap = status.gap,
                            "zkvm CL in sync"
                        );
                    }
                }
            }
        }
    }
}

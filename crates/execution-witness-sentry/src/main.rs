//! Execution witness sentry CLI.
//!
//! Monitors execution layer nodes for new blocks and fetches their execution witnesses.
//! Subscribes to CL head events to correlate EL blocks with beacon slots.

use std::{
    collections::HashMap,
    path::PathBuf,
    pin::pin,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser;
use execution_witness_sentry::{
    BlockStorage, ClClient, ClEvent, Config, ElClient, ExecutionProof, SavedProof,
    generate_random_proof, subscribe_blocks, subscribe_cl_events,
};
use futures::StreamExt;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use url::Url;

/// Execution witness sentry - monitors EL nodes and fetches witnesses.
#[derive(Parser, Debug)]
#[command(name = "execution-witness-sentry")]
#[command(about = "Monitor execution layer nodes and fetch execution witnesses")]
struct Cli {
    /// Path to configuration file.
    #[arg(long, short, default_value = "config.toml")]
    config: PathBuf,
}

/// Cached EL block data waiting for CL correlation.
struct CachedElBlock {
    block_number: u64,
    timestamp: Instant,
}

/// Cache for EL blocks keyed by block_hash.
struct ElBlockCache {
    blocks: HashMap<String, CachedElBlock>,
    max_age: Duration,
}

impl ElBlockCache {
    fn new(max_age: Duration) -> Self {
        Self {
            blocks: HashMap::new(),
            max_age,
        }
    }

    fn insert(&mut self, block_hash: String, block_number: u64, _endpoint_name: String) {
        self.blocks.insert(
            block_hash,
            CachedElBlock {
                block_number,
                timestamp: Instant::now(),
            },
        );
        self.cleanup();
    }

    fn get(&self, block_hash: &str) -> Option<&CachedElBlock> {
        self.blocks.get(block_hash)
    }

    fn remove(&mut self, block_hash: &str) -> Option<CachedElBlock> {
        self.blocks.remove(block_hash)
    }

    fn cleanup(&mut self) {
        let now = Instant::now();
        self.blocks
            .retain(|_, v| now.duration_since(v.timestamp) < self.max_age);
    }
}

/// EL event for the channel.
struct ElBlockEvent {
    endpoint_name: String,
    block_number: u64,
    block_hash: String,
}

/// CL event for the channel.
struct ClBlockEvent {
    cl_name: String,
    slot: u64,
    block_root: String,
    execution_block_hash: String,
}

/// Status of a zkvm CL node.
#[derive(Debug, Clone)]
struct ZkvmClStatus {
    name: String,
    head_slot: u64,
    gap: i64, // Negative means behind source CL
}

/// Monitor zkvm CL nodes and report their sync status.
async fn monitor_zkvm_status(
    source_client: &ClClient,
    zkvm_clients: &[(String, ClClient)],
) -> Vec<ZkvmClStatus> {
    let source_head = match source_client.get_head_slot().await {
        Ok(slot) => slot,
        Err(e) => {
            warn!(error = %e, "Failed to get source CL head");
            return vec![];
        }
    };

    let mut statuses = Vec::new();
    for (name, client) in zkvm_clients {
        match client.get_head_slot().await {
            Ok(head_slot) => {
                let gap = head_slot as i64 - source_head as i64;
                statuses.push(ZkvmClStatus {
                    name: name.clone(),
                    head_slot,
                    gap,
                });
            }
            Err(e) => {
                warn!(name = %name, error = %e, "Failed to get zkvm CL head");
            }
        }
    }

    statuses
}

/// Backfill proofs for a zkvm CL that is behind.
/// First tries to use saved proofs from disk, falls back to generating new ones.
/// Returns the number of proofs submitted.
async fn backfill_proofs(
    source_client: &ClClient,
    zkvm_client: &ClClient,
    zkvm_name: &str,
    num_proofs: usize,
    max_slots: u64,
    storage: Option<&BlockStorage>,
) -> usize {
    // Get the zkvm CL's current head
    let zkvm_head = match zkvm_client.get_head_slot().await {
        Ok(slot) => slot,
        Err(e) => {
            warn!(name = %zkvm_name, error = %e, "Failed to get zkvm CL head for backfill");
            return 0;
        }
    };

    // Get source CL head
    let source_head = match source_client.get_head_slot().await {
        Ok(slot) => slot,
        Err(e) => {
            warn!(error = %e, "Failed to get source CL head for backfill");
            return 0;
        }
    };

    if zkvm_head >= source_head {
        return 0; // Already caught up
    }

    let gap = source_head - zkvm_head;
    let slots_to_check = gap.min(max_slots);

    info!(
        name = %zkvm_name,
        zkvm_head = zkvm_head,
        source_head = source_head,
        gap = gap,
        checking = slots_to_check,
        "Backfilling proofs"
    );

    let mut proofs_submitted = 0;

    // Iterate through slots from zkvm_head + 1 to zkvm_head + slots_to_check
    for slot in (zkvm_head + 1)..=(zkvm_head + slots_to_check) {
        // First try to load saved proofs from disk
        if let Some(storage) = storage
            && let Ok(Some((_metadata, saved_proofs))) = storage.load_proofs_by_slot(slot)
            && !saved_proofs.is_empty()
        {
            debug!(
                slot = slot,
                num_proofs = saved_proofs.len(),
                "Using saved proofs from disk"
            );

            for saved_proof in &saved_proofs {
                let proof = ExecutionProof {
                    proof_id: saved_proof.proof_id,
                    slot: saved_proof.slot,
                    block_hash: saved_proof.block_hash.clone(),
                    block_root: saved_proof.block_root.clone(),
                    proof_data: saved_proof.proof_data.clone(),
                };

                match zkvm_client.submit_execution_proof(&proof).await {
                    Ok(()) => {
                        debug!(
                            name = %zkvm_name,
                            slot = slot,
                            proof_id = saved_proof.proof_id,
                            "Backfill proof submitted (from disk)"
                        );
                        proofs_submitted += 1;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if !msg.contains("already known") {
                            debug!(
                                name = %zkvm_name,
                                slot = slot,
                                proof_id = saved_proof.proof_id,
                                error = %e,
                                "Backfill proof failed"
                            );
                        }
                    }
                }
            }
            continue; // Move to next slot
        }

        // No saved proofs, fetch block info and generate new proofs
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

        // Only submit proofs for blocks with execution payloads
        let Some(exec_hash) = block_info.execution_block_hash else {
            debug!(slot = slot, "No execution payload, skipping");
            continue;
        };

        // Generate and submit proofs
        for proof_id in 0..num_proofs {
            let proof = ExecutionProof {
                proof_id: proof_id as u8,
                slot,
                block_hash: exec_hash.clone(),
                block_root: block_info.block_root.clone(),
                proof_data: generate_random_proof(proof_id as u32),
            };

            match zkvm_client.submit_execution_proof(&proof).await {
                Ok(()) => {
                    debug!(
                        name = %zkvm_name,
                        slot = slot,
                        proof_id = proof_id,
                        "Backfill proof submitted (generated)"
                    );
                    proofs_submitted += 1;
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("already known") {
                        debug!(
                            name = %zkvm_name,
                            slot = slot,
                            proof_id = proof_id,
                            error = %e,
                            "Backfill proof failed"
                        );
                    }
                }
            }
        }
    }

    if proofs_submitted > 0 {
        info!(
            name = %zkvm_name,
            proofs_submitted = proofs_submitted,
            "Backfill complete"
        );
    }

    proofs_submitted
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("execution_witness_sentry=info".parse()?),
        )
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

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

    // Set up CL clients - separate zkvm targets from event sources
    let mut zkvm_clients: Vec<(String, ClClient)> = Vec::new(); // zkvm-enabled nodes for proof submission
    let mut event_source_client: Option<(String, String, ClClient)> = None; // First available CL for events

    for endpoint in config.cl_endpoints {
        let url = match Url::parse(&endpoint.url) {
            Ok(u) => u,
            Err(e) => {
                warn!(name = %endpoint.name, error = %e, "Invalid CL endpoint URL");
                continue;
            }
        };
        let client = ClClient::new(url);

        match client.is_zkvm_enabled().await {
            Ok(true) => {
                info!(name = %endpoint.name, "CL endpoint has zkvm enabled (proof target)");
                zkvm_clients.push((endpoint.name.clone(), client));
            }
            Ok(false) => {
                info!(name = %endpoint.name, "CL endpoint does not have zkvm enabled");
                // Use first non-zkvm CL as event source
                if event_source_client.is_none() {
                    info!(name = %endpoint.name, "Using as event source");
                    event_source_client =
                        Some((endpoint.name.clone(), endpoint.url.clone(), client));
                }
            }
            Err(e) => {
                warn!(name = %endpoint.name, error = %e, "Failed to check zkvm status");
            }
        }
    }

    info!(
        zkvm_targets = zkvm_clients.len(),
        "zkvm-enabled CL endpoints configured"
    );

    let Some(event_source) = event_source_client else {
        error!("No non-zkvm CL endpoint available for event source");
        return Ok(());
    };
    info!(name = %event_source.0, "CL event source configured");

    let num_proofs = config.num_proofs.unwrap_or(2) as usize;

    // Set up block storage
    let storage = config.output_dir.as_ref().map(|dir| {
        BlockStorage::new(
            dir,
            config.chain.as_deref().unwrap_or("unknown"),
            config.retain,
        )
    });

    // Cache for EL blocks (keyed by block_hash)
    let el_cache = Arc::new(Mutex::new(ElBlockCache::new(Duration::from_secs(60))));

    // Channels for events
    let (el_tx, mut el_rx) = tokio::sync::mpsc::channel::<ElBlockEvent>(100);
    let (cl_tx, mut cl_rx) = tokio::sync::mpsc::channel::<ClBlockEvent>(100);

    // Spawn EL subscription tasks
    for endpoint in config.el_endpoints.clone() {
        let tx = el_tx.clone();
        let name = endpoint.name.clone();
        let ws_url = endpoint.ws_url.clone();

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
                        let event = ElBlockEvent {
                            endpoint_name: name.clone(),
                            block_number: header.number,
                            block_hash: format!("{:?}", header.hash),
                        };
                        if tx.send(event).await.is_err() {
                            break;
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

    let (es_name, es_url, es_client) = event_source;
    let source_client_for_monitor = es_client.clone();

    // Spawn CL subscription task for the event source (non-zkvm CL)
    {
        let tx = cl_tx.clone();

        tokio::spawn(async move {
            info!(name = %es_name, "Connecting to CL SSE");

            let stream = match subscribe_cl_events(&es_url) {
                Ok(s) => s,
                Err(e) => {
                    error!(name = %es_name, error = %e, "Failed to subscribe to CL events");
                    return;
                }
            };

            info!(name = %es_name, "Subscribed to CL head events");
            let mut stream = pin!(stream);

            while let Some(result) = stream.next().await {
                match result {
                    Ok(ClEvent::Head(head)) => {
                        let slot: u64 = match head.slot.parse() {
                            Ok(slot) => slot,
                            Err(e) => {
                                warn!(
                                    name = %es_name,
                                    error = %e,
                                    slot = %head.slot,
                                    "Invalid head slot value"
                                );
                                continue;
                            }
                        };
                        let block_root = head.block.clone();

                        // Fetch the execution block hash for this beacon block
                        let exec_hash = match es_client.get_block_execution_hash(&block_root).await
                        {
                            Ok(Some(hash)) => hash,
                            Ok(None) => {
                                debug!(name = %es_name, slot = slot, "No execution hash for block");
                                continue;
                            }
                            Err(e) => {
                                debug!(name = %es_name, error = %e, "Failed to get execution hash");
                                continue;
                            }
                        };

                        let event = ClBlockEvent {
                            cl_name: es_name.clone(),
                            slot,
                            block_root,
                            execution_block_hash: exec_hash,
                        };
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Ok(ClEvent::Block(_)) => {
                        // We use head events primarily
                    }
                    Err(e) => {
                        error!(name = %es_name, error = %e, "CL stream error");
                    }
                }
            }
            warn!(name = %es_name, "CL SSE stream ended");
        });
    }

    drop(el_tx);
    drop(cl_tx);

    // Create a timer for periodic monitoring and backfill (500ms for fast catch-up)
    let mut monitor_interval = tokio::time::interval(Duration::from_millis(500));
    monitor_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!("Waiting for events (with monitoring every 500ms)");

    // Process events from both EL and CL
    loop {
        tokio::select! {
            // Periodic monitoring and backfill
            _ = monitor_interval.tick() => {
                // Monitor zkvm CL status
                let statuses = monitor_zkvm_status(&source_client_for_monitor, &zkvm_clients).await;

                for status in &statuses {
                    if status.gap < -5 {
                        // More than 5 slots behind - log warning and backfill
                        warn!(
                            name = %status.name,
                            head_slot = status.head_slot,
                            gap = status.gap,
                            "zkvm CL is behind, starting backfill"
                        );

                        // Find the client and backfill
                        if let Some((_, client)) = zkvm_clients.iter().find(|(n, _)| n == &status.name) {
                            backfill_proofs(
                                &source_client_for_monitor,
                                client,
                                &status.name,
                                num_proofs,
                                20, // Max 20 slots per backfill cycle
                                storage.as_ref(),
                            ).await;
                        }
                    } else if status.gap < 0 {
                        // Slightly behind - just log
                        debug!(
                            name = %status.name,
                            head_slot = status.head_slot,
                            gap = status.gap,
                            "zkvm CL slightly behind"
                        );
                    } else {
                        // In sync or ahead
                        debug!(
                            name = %status.name,
                            head_slot = status.head_slot,
                            gap = status.gap,
                            "zkvm CL in sync"
                        );
                    }
                }
            }

            Some(el_event) = el_rx.recv() => {
                info!(
                    name = %el_event.endpoint_name,
                    number = el_event.block_number,
                    hash = %el_event.block_hash,
                    "EL block received"
                );

                // Find the endpoint and fetch block + witness
                let Some(endpoint) = config.el_endpoints.iter().find(|e| e.name == el_event.endpoint_name) else {
                    continue;
                };

                let Ok(url) = Url::parse(&endpoint.url) else {
                    continue;
                };
                let el_client = ElClient::new(url);

                // Fetch block and witness
                let (block, gzipped_block) = match el_client.get_block_by_hash(&el_event.block_hash).await {
                    Ok(Some(data)) => data,
                    Ok(None) => {
                        warn!(number = el_event.block_number, "Block not found");
                        continue;
                    }
                    Err(e) => {
                        error!(number = el_event.block_number, error = %e, "Failed to fetch block");
                        continue;
                    }
                };

                let (witness, gzipped_witness) = match el_client.get_execution_witness(el_event.block_number).await {
                    Ok(Some(data)) => data,
                    Ok(None) => {
                        warn!(number = el_event.block_number, "Witness not found");
                        continue;
                    }
                    Err(e) => {
                        error!(number = el_event.block_number, error = %e, "Failed to fetch witness");
                        continue;
                    }
                };

                info!(
                    number = el_event.block_number,
                    block_gzipped = gzipped_block.len(),
                    witness_gzipped = gzipped_witness.len(),
                    "Fetched block and witness"
                );

                // Save to disk if storage is configured
                if let Some(ref storage) = storage {
                    let combined = serde_json::json!({
                        "block": block,
                        "witness": witness,
                    });
                    let combined_bytes = serde_json::to_vec(&combined)?;
                    let gzipped_combined = execution_witness_sentry::compress_gzip(&combined_bytes)?;

                    if let Err(e) = storage.save_block(&block, &gzipped_combined) {
                        error!(error = %e, "Failed to save block");
                    } else {
                        info!(
                            number = el_event.block_number,
                            separate = gzipped_block.len() + gzipped_witness.len(),
                            combined = gzipped_combined.len(),
                            "Saved"
                        );
                    }
                }

                // Cache the EL block for correlation with CL events
                let mut cache = el_cache.lock().await;
                cache.insert(
                    el_event.block_hash.clone(),
                    el_event.block_number,
                    el_event.endpoint_name.clone(),
                );
            }

            Some(cl_event) = cl_rx.recv() => {
                info!(
                    source = %cl_event.cl_name,
                    slot = cl_event.slot,
                    block_root = %cl_event.block_root,
                    exec_hash = %cl_event.execution_block_hash,
                    "CL head event received"
                );

                // Check if we have the EL block cached
                let cached_block_number = {
                    let cache = el_cache.lock().await;
                    cache.get(&cl_event.execution_block_hash).map(|c| c.block_number)
                };

                if cached_block_number.is_none() {
                    debug!(
                        exec_hash = %cl_event.execution_block_hash,
                        "EL block not in cache, skipping proof submission"
                    );
                    continue;
                }
                let block_number = cached_block_number.unwrap();

                // Generate proofs once (for all CLs and for saving)
                let mut generated_proofs: Vec<SavedProof> = Vec::new();
                for proof_id in 0..num_proofs {
                    generated_proofs.push(SavedProof {
                        proof_id: proof_id as u8,
                        slot: cl_event.slot,
                        block_hash: cl_event.execution_block_hash.clone(),
                        block_root: cl_event.block_root.clone(),
                        proof_data: generate_random_proof(proof_id as u32),
                    });
                }

                // Save proofs to disk for backfill
                if let Some(ref storage) = storage {
                    if let Err(e) = storage.save_proofs(
                        block_number,
                        cl_event.slot,
                        &cl_event.block_root,
                        &cl_event.execution_block_hash,
                        &generated_proofs,
                    ) {
                        warn!(slot = cl_event.slot, error = %e, "Failed to save proofs to disk");
                    } else {
                        debug!(slot = cl_event.slot, block_number = block_number, "Saved proofs to disk");
                    }
                }

                // Submit proofs to ALL zkvm-enabled CL clients
                for (cl_name, cl_client) in &zkvm_clients {
                    for saved_proof in &generated_proofs {
                        let proof = ExecutionProof {
                            proof_id: saved_proof.proof_id,
                            slot: saved_proof.slot,
                            block_hash: saved_proof.block_hash.clone(),
                            block_root: saved_proof.block_root.clone(),
                            proof_data: saved_proof.proof_data.clone(),
                        };

                        match cl_client.submit_execution_proof(&proof).await {
                            Ok(()) => {
                                info!(
                                    cl = %cl_name,
                                    slot = cl_event.slot,
                                    proof_id = saved_proof.proof_id,
                                    "Proof submitted"
                                );
                            }
                            Err(e) => {
                                debug!(
                                    cl = %cl_name,
                                    slot = cl_event.slot,
                                    proof_id = saved_proof.proof_id,
                                    error = %e,
                                    "Proof submission failed"
                                );
                            }
                        }
                    }
                }

                // Remove from cache after submission
                let mut cache = el_cache.lock().await;
                cache.remove(&cl_event.execution_block_hash);
            }

            else => break,
        }
    }

    Ok(())
}

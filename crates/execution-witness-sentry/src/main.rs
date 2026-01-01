//! Execution witness sentry CLI.
//!
//! Monitors execution layer nodes for new blocks and fetches their execution witnesses.

use std::collections::HashSet;
use std::pin::pin;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use futures::StreamExt;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};
use url::Url;

use execution_witness_sentry::{
    subscribe_blocks, BlockStorage, ClClient, Config, ElClient, ExecutionProof,
    generate_random_proof,
};

/// Execution witness sentry - monitors EL nodes and fetches witnesses.
#[derive(Parser, Debug)]
#[command(name = "execution-witness-sentry")]
#[command(about = "Monitor execution layer nodes and fetch execution witnesses")]
struct Cli {
    /// Path to configuration file.
    #[arg(long, short, default_value = "config.toml")]
    config: PathBuf,
}

/// Event sent from subscription tasks to the main processing loop.
struct BlockEvent {
    /// Name of the endpoint that reported this block.
    endpoint_name: String,
    /// Block number.
    number: u64,
    /// Block hash.
    hash: String,
}

/// Tracks which blocks have already been processed.
#[derive(Default)]
struct SeenBlocks {
    seen: HashSet<String>,
}

impl SeenBlocks {
    /// Returns `true` if this is a new block (not seen before).
    fn is_new(&mut self, block_hash: &str) -> bool {
        self.seen.insert(block_hash.to_string())
    }
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

    info!(endpoints = config.endpoints.len(), "Loaded configuration");
    for endpoint in &config.endpoints {
        info!(
            name = %endpoint.name,
            el_url = %endpoint.el_url,
            el_ws_url = %endpoint.el_ws_url,
            "EL endpoint configured"
        );
    }

    // Set up CL clients for proof submission
    let cl_clients: Vec<(String, ClClient)> = config
        .cl_endpoints
        .as_ref()
        .map(|endpoints| {
            endpoints
                .iter()
                .filter_map(|e| {
                    Url::parse(&e.url)
                        .ok()
                        .map(|url| (e.name.clone(), ClClient::new(url)))
                })
                .collect()
        })
        .unwrap_or_default();

    info!(cl_endpoints = cl_clients.len(), "CL endpoints configured");
    for (name, _) in &cl_clients {
        info!(name = %name, "CL endpoint configured");
    }

    let num_proofs = config.num_proofs.unwrap_or(2);

    // Set up block storage
    let storage = config.output_dir.as_ref().map(|dir| {
        BlockStorage::new(
            dir,
            config.chain.as_deref().unwrap_or("unknown"),
            config.retain,
        )
    });

    let seen_blocks = Arc::new(Mutex::new(SeenBlocks::default()));
    let (tx, mut rx) = mpsc::channel::<BlockEvent>(100);

    // Spawn subscription tasks for each endpoint
    for endpoint in config.endpoints.clone() {
        let tx = tx.clone();
        let name = endpoint.name.clone();
        let ws_url = endpoint.el_ws_url.clone();

        tokio::spawn(async move {
            info!(name = %name, "Connecting to EL WebSocket");

            let stream = match subscribe_blocks(&ws_url).await {
                Ok(s) => s,
                Err(e) => {
                    error!(name = %name, error = %e, "Failed to subscribe");
                    return;
                }
            };

            info!(name = %name, "Subscribed to newHeads");

            let mut stream = pin!(stream);

            while let Some(result) = stream.next().await {
                match result {
                    Ok(header) => {
                        let event = BlockEvent {
                            endpoint_name: name.clone(),
                            number: header.number,
                            hash: format!("{:?}", header.hash),
                        };
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        error!(name = %name, error = %e, "Stream error");
                    }
                }
            }
            warn!(name = %name, "WebSocket stream ended");
        });
    }

    drop(tx);

    info!("Waiting for events");

    // Process incoming block events
    while let Some(event) = rx.recv().await {
        let mut seen = seen_blocks.lock().await;
        let is_new = seen.is_new(&event.hash);
        drop(seen);

        if !is_new {
            debug!(
                name = %event.endpoint_name,
                number = event.number,
                "Already seen"
            );
            continue;
        }

        info!(
            name = %event.endpoint_name,
            number = event.number,
            hash = %event.hash,
            "New block"
        );

        // Find the endpoint that reported this block
        let endpoint = match config.endpoints.iter().find(|e| e.name == event.endpoint_name) {
            Some(e) => e,
            None => continue,
        };

        let el_url = match Url::parse(&endpoint.el_url) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let el_client = ElClient::new(el_url);

        // Fetch block and witness
        let (block, gzipped_block) = match el_client.get_block_by_hash(&event.hash).await {
            Ok(Some(data)) => data,
            Ok(None) => {
                warn!(number = event.number, "Block not found");
                continue;
            }
            Err(e) => {
                error!(number = event.number, error = %e, "Failed to fetch block");
                continue;
            }
        };

        let (witness, gzipped_witness) = match el_client.get_execution_witness(event.number).await {
            Ok(Some(data)) => data,
            Ok(None) => {
                warn!(number = event.number, "Witness not found");
                continue;
            }
            Err(e) => {
                error!(number = event.number, error = %e, "Failed to fetch witness");
                continue;
            }
        };

        info!(
            number = event.number,
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
                    number = event.number,
                    separate = gzipped_block.len() + gzipped_witness.len(),
                    combined = gzipped_combined.len(),
                    "Saved"
                );
            }
        }

        // Submit proofs to CL endpoints
        for (cl_name, cl_client) in &cl_clients {
            let sync_status = match cl_client.get_syncing().await {
                Ok(s) => s,
                Err(e) => {
                    error!(cl = %cl_name, error = %e, "Failed to get sync status");
                    continue;
                }
            };

            let head_slot: u64 = sync_status.data.head_slot.parse().unwrap_or(0);

            let block_root = match cl_client.get_block_header(head_slot).await {
                Ok(Some(header)) => header.data.root,
                Ok(None) => continue,
                Err(e) => {
                    debug!(cl = %cl_name, error = %e, "Failed to get block header");
                    continue;
                }
            };

            for proof_id in 0..num_proofs {
                let proof = ExecutionProof {
                    proof_id,
                    slot: head_slot.to_string(),
                    block_hash: event.hash.clone(),
                    block_root: block_root.clone(),
                    proof_data: generate_random_proof(proof_id),
                };

                match cl_client.submit_execution_proof(&proof).await {
                    Ok(()) => {
                        info!(
                            cl = %cl_name,
                            slot = head_slot,
                            proof_id = proof_id,
                            "Proof submitted"
                        );
                    }
                    Err(e) => {
                        debug!(
                            cl = %cl_name,
                            slot = head_slot,
                            proof_id = proof_id,
                            error = %e,
                            "Proof submission failed"
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

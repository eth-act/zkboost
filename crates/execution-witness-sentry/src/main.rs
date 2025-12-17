//! Execution witness sentry CLI.
//!
//! Monitors execution layer nodes for new blocks and fetches their execution witnesses.

use std::collections::HashSet;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;

use clap::Parser;
use futures::StreamExt;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};
use url::Url;

use execution_witness_sentry::{subscribe_blocks, BlockStorage, Config, ElClient};

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
            ws_url = %endpoint.el_ws_url,
            "Configured endpoint"
        );
    }

    run(config).await
}

async fn run(config: Config) -> anyhow::Result<()> {
    let seen_blocks = Arc::new(Mutex::new(SeenBlocks::default()));
    let (tx, rx) = mpsc::channel::<BlockEvent>(100);

    // Spawn subscription tasks for each endpoint.
    for endpoint in &config.endpoints {
        spawn_subscription_task(endpoint.name.clone(), endpoint.el_ws_url.clone(), tx.clone());
    }

    // Drop our sender so the channel closes when all subscription tasks end.
    drop(tx);

    // Process incoming block events.
    process_blocks(config, seen_blocks, rx).await;

    Ok(())
}

/// Spawn a task that subscribes to new block headers and forwards them to the channel.
fn spawn_subscription_task(name: String, ws_url: String, tx: mpsc::Sender<BlockEvent>) {
    tokio::spawn(async move {
        info!(name = %name, "Connecting to WebSocket");

        let stream = match subscribe_blocks(&ws_url).await {
            Ok(s) => s,
            Err(e) => {
                error!(name = %name, error = %e, "Failed to subscribe");
                return;
            }
        };

        info!(name = %name, "Subscribed to newHeads");

        let mut stream = pin!(stream);
        while let Some(header) = stream.next().await {
            let event = BlockEvent {
                endpoint_name: name.clone(),
                number: header.number,
                hash: format!("{:?}", header.hash),
            };

            if tx.send(event).await.is_err() {
                break;
            }
        }

        warn!(name = %name, "WebSocket stream ended");
    });
}

/// Process incoming block events: fetch data and save to storage.
async fn process_blocks(
    config: Config,
    seen_blocks: Arc<Mutex<SeenBlocks>>,
    mut rx: mpsc::Receiver<BlockEvent>,
) {
    // Set up storage if configured.
    let storage = config.output_dir.as_ref().map(|dir| {
        BlockStorage::new(dir, config.chain.as_deref().unwrap_or("unknown"), config.retain)
    });

    info!("Waiting for blocks");

    while let Some(event) = rx.recv().await {
        // Check if we've already seen this block.
        let is_new = seen_blocks.lock().await.is_new(&event.hash);

        if !is_new {
            debug!(
                name = %event.endpoint_name,
                number = event.number,
                "Block already seen"
            );
            continue;
        }

        info!(
            name = %event.endpoint_name,
            number = event.number,
            hash = %event.hash,
            "New block"
        );

        // Find the endpoint configuration.
        let Some(endpoint) = config.endpoints.iter().find(|e| e.name == event.endpoint_name) else {
            error!(name = %event.endpoint_name, "Endpoint not found in config");
            continue;
        };

        // Process the block.
        if let Err(e) = process_block(endpoint, &event, storage.as_ref()).await {
            error!(
                name = %event.endpoint_name,
                number = event.number,
                error = %e,
                "Failed to process block"
            );
        }
    }
}

/// Fetch block and witness data, then save to storage.
async fn process_block(
    endpoint: &execution_witness_sentry::Endpoint,
    event: &BlockEvent,
    storage: Option<&BlockStorage>,
) -> anyhow::Result<()> {
    let el_url: Url = endpoint.el_url.parse()?;
    let client = ElClient::new(el_url);

    // Fetch the block.
    let Some(block) = client.get_block_by_hash(&event.hash).await? else {
        warn!(number = event.number, "Block not found");
        return Ok(());
    };

    debug!(
        number = event.number,
        txs = block.transactions.len(),
        "Fetched block"
    );

    // Fetch the execution witness.
    let Some(witness) = client.get_execution_witness(event.number).await? else {
        warn!(number = event.number, "Witness not found");
        return Ok(());
    };

    info!(number = event.number, "Fetched witness");

    // Save to storage if configured.
    if let Some(storage) = storage {
        storage.save(event.number, &event.hash, &block, &witness)?;
    }

    Ok(())
}

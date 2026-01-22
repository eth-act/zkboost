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

use std::{num::NonZeroUsize, path::PathBuf, sync::Arc};

use anyhow::bail;
use clap::Parser;
use execution_witness_sentry::{
    BlockStorage, ClClient, Config, ElClient,
    service::{
        backfill::BackfillService,
        cl_event::ClEventService,
        el_data::{ElDataService, ElDataServiceMessage},
        el_event::ElEventService,
        proof::{ProofService, ProofServiceMessage},
    },
};
use futures::future::select_all;
use lru::LruCache;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "execution-witness-sentry")]
#[command(about = "Monitor execution layer nodes and fetch execution witnesses")]
struct Cli {
    #[arg(long, short, default_value = "config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let config = Arc::new(Config::load(&cli.config)?);

    // Initialize EL clients.

    info!(
        el_endpoints = config.el_endpoints.len(),
        "Loaded configuration"
    );
    let el_clients: Vec<Arc<ElClient>> = config
        .el_endpoints
        .iter()
        .map(|endpoint| {
            info!(
                name = %endpoint.name,
                url = %endpoint.url,
                ws_url = %endpoint.ws_url,
                "EL endpoint configured"
            );

            Arc::new(ElClient::new(endpoint.name.clone(), endpoint.url.clone()))
        })
        .collect();

    // Get chain config.

    let mut chain_config = None;
    for el_client in &el_clients {
        if let Ok(Some(c)) = el_client.get_chain_config().await {
            chain_config = Some(c);
            break;
        } else {
            warn!(
                name = %el_client.name(),
                url = %el_client.url(),
                "Failed to get chain config",
            )
        };
    }
    let Some(chain_config) = chain_config else {
        bail!("Failed to get chain config from any EL endpoint");
    };

    // Initialize CL clients.

    let mut zkvm_enabled_cl_clients: Vec<Arc<ClClient>> = Vec::new();
    let mut source_cl_client: Option<Arc<ClClient>> = None;

    for endpoint in &config.cl_endpoints {
        let client = ClClient::new(endpoint.name.clone(), endpoint.url.clone());

        match client.is_zkvm_enabled().await {
            Ok(true) => {
                info!(name = %endpoint.name, "CL endpoint has zkvm enabled (proof target)");
                zkvm_enabled_cl_clients.push(Arc::new(client));
            }
            Ok(false) => {
                info!(name = %endpoint.name, "CL endpoint does not have zkvm enabled");
                if source_cl_client.is_none() {
                    info!(name = %endpoint.name, "Using as event source");
                    source_cl_client = Some(Arc::new(client));
                }
            }
            Err(e) => {
                warn!(name = %endpoint.name, error = %e, "Failed to check zkvm status");
            }
        }
    }

    info!(
        zkvm_enabled_cl_clients = zkvm_enabled_cl_clients.len(),
        "zkvm-enabled CL endpoints configured"
    );

    let Some(source_cl_client) = source_cl_client else {
        bail!("No non-zkvm CL endpoint available for event source");
    };
    info!(name = %source_cl_client.name(), "CL event source configured");

    let block_cache = Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(128).unwrap())));
    let storage: Option<Arc<Mutex<BlockStorage>>> = config.output_dir.as_ref().map(|dir| {
        Arc::new(Mutex::new(BlockStorage::new(
            dir,
            config.chain.as_deref().unwrap_or("unknown"),
            config.retain,
        )))
    });

    let (proof_tx, proof_rx) = tokio::sync::mpsc::channel::<ProofServiceMessage>(1024);
    let (el_data_tx, el_data_rx) = tokio::sync::mpsc::channel::<ElDataServiceMessage>(1024);

    let shutdown_token = CancellationToken::new();

    let mut handles = Vec::new();

    // Start CL event listening service.

    {
        let cl_event_service =
            ClEventService::new(source_cl_client.clone(), storage.clone(), proof_tx.clone());
        let shutdown_token = shutdown_token.clone();

        handles.push(tokio::spawn(async move {
            cl_event_service.run(shutdown_token).await;
        }));
    }

    // Start EL event listening services.

    for endpoint in &config.el_endpoints {
        let el_event_service = ElEventService::new(endpoint.clone(), el_data_tx.clone());
        let shutdown_token = shutdown_token.clone();

        handles.push(tokio::spawn(async move {
            el_event_service.run(shutdown_token).await;
        }));
    }

    // Start EL data service.

    {
        let el_data_service = ElDataService::new(
            el_clients.clone(),
            block_cache.clone(),
            storage.clone(),
            el_data_rx,
            proof_tx.clone(),
        );
        let shutdown_token = shutdown_token.clone();

        handles.push(tokio::spawn(async move {
            el_data_service.run(shutdown_token).await;
        }));
    }

    // Start proof service.

    {
        let proof_service = ProofService::new(
            config.proof_engine.clone(),
            chain_config,
            zkvm_enabled_cl_clients.clone(),
            block_cache.clone(),
            storage.clone(),
            proof_rx,
        )?;
        let shutdown_token = shutdown_token.clone();

        handles.push(tokio::spawn(async move {
            if let Err(e) = proof_service.run(shutdown_token).await {
                error!(error = %e, "ProofService error");
            }
        }));
    }

    // Start backfilling service.

    {
        let interval_ms = 500;
        let backfill_service = BackfillService::new(
            source_cl_client,
            zkvm_enabled_cl_clients,
            block_cache,
            storage,
            proof_tx,
            el_data_tx,
            interval_ms,
        );
        let shutdown_token = shutdown_token.clone();

        handles.push(tokio::spawn(async move {
            backfill_service.run(shutdown_token).await;
        }));
    }

    info!("All services started, waiting for shutdown signal");

    let mut signals: Vec<_> = [SignalKind::interrupt(), SignalKind::terminate()]
        .into_iter()
        .filter_map(|kind| signal(kind).ok())
        .collect();

    if signals.is_empty() {
        bail!("No shutdown signals could be registered");
    }

    let _ = select_all(signals.iter_mut().map(|s| Box::pin(s.recv()))).await;

    info!("Received shutdown signal, shutting down");

    shutdown_token.cancel();

    for handle in handles {
        let _ = handle.await;
    }

    info!("All services stopped, exiting");

    Ok(())
}

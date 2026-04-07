//! zkboost proof node.
//!
//! Orchestrates witness fetching, proof generation, and an HTTP API for
//! submitting proof requests and retrieving completed proofs.

use std::path::PathBuf;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use zkboost_server::{
    config::Config,
    metrics::{init_metrics, spawn_upkeep},
    server::zkBoostServer,
};

#[derive(Parser)]
struct Cli {
    /// Path to configuration file.
    #[arg(long, short)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(feature = "otel")]
    let (telemetry_provider, otel_layer) = zkboost_server::otel::init();
    #[cfg(not(feature = "otel"))]
    let otel_layer: Option<tracing_subscriber::layer::Identity> = None;

    tracing_subscriber::registry()
        .with(otel_layer)
        .with(tracing_subscriber::fmt::layer().compact())
        .with(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let metrics = init_metrics();
    spawn_upkeep(metrics.clone());

    let config = Config::load(&cli.config)?;
    info!(
        port = config.port,
        el_endpoint = %config.el_endpoint,
        zkvm_count = config.zkvm.len(),
        "configuration loaded"
    );

    let shutdown_token = CancellationToken::new();

    let server = zkBoostServer::new(config, metrics).await?;
    let (_addr, handles) = server.run(shutdown_token.clone()).await?;

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = sigint.recv() => info!("received SIGINT"),
        _ = sigterm.recv() => info!("received SIGTERM"),
    }

    info!("shutting down");
    shutdown_token.cancel();

    for handle in handles {
        if let Err(error) = handle.await {
            error!(error = %error, "service task failed");
        }
    }

    #[cfg(feature = "otel")]
    if let Some(provider) = telemetry_provider
        && let Err(error) = provider.shutdown()
    {
        error!(error = %error, "otel provider shutdown failed");
    }

    info!("all services stopped");
    Ok(())
}

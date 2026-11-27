//! zkboost, an Engine API proxy that proves execution payloads.

use std::path::PathBuf;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::info;
use zkboost_server::{
    Server,
    config::Config,
    observability::{metrics, telemetry},
};

#[derive(Parser)]
struct Cli {
    /// Path of the configuration file.
    #[arg(long, short)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _telemetry = telemetry::init();
    let config = Config::load(&Cli::parse().config)?;
    let server = Server::bind(config, metrics::install()).await?;

    let shutdown = CancellationToken::new();
    let (mut sigint, mut sigterm) = (
        signal(SignalKind::interrupt())?,
        signal(SignalKind::terminate())?,
    );
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            tokio::select! {
                _ = sigint.recv() => info!("received SIGINT"),
                _ = sigterm.recv() => info!("received SIGTERM"),
            }
            shutdown.cancel();
        }
    });
    server.run(shutdown).await
}

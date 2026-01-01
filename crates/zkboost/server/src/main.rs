//! zkVM execution and proving service.
//!
//! Provides HTTP endpoints for executing zkVM programs, generating proofs, and verifying
//! proofs using various zkVM backends (SP1, OpenVM, Zisk, etc.).
//!
//! ## Endpoints
//!
//! - `POST /execute` -  Execute a program without generating a proof
//! - `POST /prove` -  Generate a proof for a program execution
//! - `POST /verify` -  Verify a proof
//! - `GET /info` -  Get server hardware and system information
//!
//! ## Configuration
//!
//! The server is configured via a TOML/YAML file that specifies which zkVM programs to load.
//! See [`config::Config`] for details.
//!
//! ## Usage
//!
//! ```bash
//! zkboost --config config.toml --port 3001
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
};
use tracing::info;
use zkboost_server_config::Config;

use crate::app::{AppState, app};

mod app;
mod metrics;

#[cfg(test)]
mod mock;

/// Command-line interface for the zkboost server.
#[derive(Parser, Debug)]
#[command(name = "zkboost")]
#[command(about = "zkVM execution and proving service", long_about = None)]
pub struct Cli {
    /// Config file path.
    #[arg(long)]
    pub config: PathBuf,

    /// Port to listen on.
    #[arg(long, default_value = "3001")]
    pub port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .with_level(true)
        .with_thread_names(true)
        .with_ansi(true)
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    let state = AppState::new(&config).await?;
    let router = app(state);

    let addr: SocketAddr = format!("0.0.0.0:{}", cli.port).parse()?;
    info!("zkboost listening on {addr}");

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => info!("Received SIGTERM, starting graceful shutdown"),
        _ = sigint.recv() => info!("Received SIGINT (Ctrl-C), starting graceful shutdown"),
    }
}

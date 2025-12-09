#![warn(unused_crate_dependencies)]

use std::{fs, net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{
    Router,
    routing::{get, post},
};
use clap::Parser;
use ere_dockerized::{DockerizedzkVM, SerializedProgram};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::RwLock,
};
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::{
    common::{AppState, ProgramID, zkVMInstance},
    config::Config,
    endpoints::{execute_program, get_server_info, prove_program, verify_proof},
};

mod common;
mod config;
mod endpoints;

#[cfg(test)]
mod mock_zkvm;

#[derive(Parser, Debug)]
#[command(name = "zkboost")]
#[command(about = "zkVM execution and proving service", long_about = None)]
pub struct Cli {
    /// Config file path
    #[arg(long)]
    pub config: PathBuf,

    /// Port
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

    let app = init_state(&config.zkvm)?;

    let addr: SocketAddr = format!("0.0.0.0:{}", cli.port).parse()?;
    info!("zkboost listening on {addr}");

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn init_state(zkvm: &[config::zkVM]) -> anyhow::Result<Router> {
    let programs = zkvm
        .iter()
        .map(|config| Ok((ProgramID(config.program_id.clone()), init_zkvm(config)?)))
        .collect::<anyhow::Result<_>>()?;
    Ok(app(AppState {
        programs: Arc::new(RwLock::new(programs)),
    }))
}

fn init_zkvm(config: &config::zkVM) -> anyhow::Result<zkVMInstance> {
    let program = fs::read(&config.program_path)
        .map(SerializedProgram)
        .with_context(|| format!("Program not found at {}", &config.program_path.display()))?;
    let zkvm = DockerizedzkVM::new(config.kind, program, config.resource.clone())
        .with_context(|| format!("Failed to initialize DockerizedzkVM, kind {}", config.kind))?;
    Ok(zkVMInstance::new(zkvm))
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/execute", post(execute_program))
        .route("/prove", post(prove_program))
        .route("/verify", post(verify_proof))
        .route("/info", get(get_server_info))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        // 400MB limit to account for the proof size
        // and the possibly large input size
        .layer(axum::extract::DefaultBodyLimit::max(400 * 1024 * 1024))
}

async fn shutdown_signal() {
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => info!("Received SIGTERM, starting graceful shutdown"),
        _ = sigint.recv() => info!("Received SIGINT (Ctrl-C), starting graceful shutdown"),
    }
}

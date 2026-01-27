//! Integration tests for the zkboost-server HTTP API with stateless validator
//! program.

#![allow(unused_crate_dependencies)]

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::bail;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use clap::Parser;
use ere_dockerized::zkVMKind;
use ere_zkvm_interface::ProverResourceType;
use nix::sys::{prctl, signal::Signal};
use tempfile::tempdir;
use tokio::{process::Command, sync::mpsc, time::sleep};
use tower_http::trace::TraceLayer;
use tracing::info;
use zkboost_client::zkboostClient;
use zkboost_ethereum_el_config::program::download_program;
use zkboost_ethereum_el_input::ElInput;
use zkboost_ethereum_el_types::ElKind;
use zkboost_server_config::{Config, zkVMConfig};
use zkboost_types::ProofResult;

use crate::utils::{ClientSession, DockerComposeGuard, ServerResources, fetch_empty_block};

mod utils;

#[derive(Parser)]
struct Args {
    /// Path to `zkboost-server` binary.
    /// If not provided, `cargo run --release --bin zkboost-server` will be used.
    #[arg(long)]
    zkboost_server_bin: Option<PathBuf>,
    /// Execution layer client implementation (reth or ethrex)
    #[arg(long, default_value = "reth")]
    el: ElKind,
    /// zkVM to use
    #[arg(long)]
    zkvm: zkVMKind,
    /// Resource type for proving (cpu or gpu)
    #[arg(long, ignore_case = true, default_value = "cpu", value_parser = ["cpu", "gpu"])]
    resource: String,
    /// RPC URL for generating test fixtures from a live blockchain.
    /// If not provided, an empty block fixture will be used.
    #[arg(long)]
    rpc_url: Option<String>,
    /// Comma-separated HTTP headers for RPC requests e.g. `key1:value1,key2:value2`
    #[arg(long)]
    rpc_headers: Option<String>,
    /// Keep the temporary workspace directory for debugging
    #[arg(long)]
    keep_workspace: bool,
    /// Skip proving
    #[arg(long)]
    skip_prove: bool,
    /// Docker registry for Ere images
    #[arg(env = "ERE_IMAGE_REGISTRY", default_value = "ghcr.io/eth-act/ere")]
    ere_image_registry: String,
    /// GitHub token for downloading artifacts from GitHub Actions.
    /// Required when `ere-guests` dependency uses a git revision instead of a released tag.
    #[arg(env = "GITHUB_TOKEN")]
    github_token: Option<String>,
    /// Run the server using docker compose
    #[arg(long)]
    dockerized: bool,
}

impl Args {
    fn program_id(&self) -> String {
        format!("{}-{}", self.el.as_str(), self.zkvm)
    }
}

/// Download program and generate config file.
async fn generate_config(
    args: &Args,
    workspace: &Path,
    port: u16,
    webhook_url: &str,
) -> anyhow::Result<String> {
    info!("Generating config...");

    let program = download_program(
        args.el,
        args.zkvm,
        args.github_token.as_deref(),
        workspace.join("program"),
        args.keep_workspace,
    )
    .await?;

    let resource = match args.resource.to_lowercase().as_str() {
        "cpu" => ProverResourceType::Cpu,
        "gpu" => ProverResourceType::Gpu,
        _ => bail!("Unsupported resource type: {}", args.resource),
    };

    let config = Config {
        port,
        webhook_url: webhook_url.to_string(),
        zkvm: vec![zkVMConfig::Docker {
            kind: args.zkvm,
            resource,
            program_id: args.program_id().into(),
            program,
        }],
    };

    let config_path = workspace.join("config.toml");
    tokio::fs::write(&config_path, config.to_toml()?).await?;

    info!("Successfully generated config");

    Ok(config_path.to_string_lossy().to_string())
}

/// Start `zkboost-server` with stateless validator program.
async fn start_zkboost_server(
    args: &Args,
    config_path: &str,
    zkboost_server_port: u16,
) -> anyhow::Result<ClientSession> {
    if args.dockerized {
        start_zkboost_server_dockerized(config_path, zkboost_server_port).await
    } else {
        start_zkboost_server_raw(args, config_path, zkboost_server_port).await
    }
}

/// Start `zkboost-server` using docker compose.
async fn start_zkboost_server_dockerized(
    config_path: &str,
    zkboost_server_port: u16,
) -> anyhow::Result<ClientSession> {
    info!("Starting server via docker compose...");

    let docker_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("docker");

    let guard = DockerComposeGuard {
        docker_dir: docker_dir.clone(),
    };

    let docker_config_path = docker_dir.join("config.toml");
    tokio::fs::copy(config_path, &docker_config_path).await?;

    let status = Command::new("docker")
        .args(["compose", "up", "-d", "zkboost"])
        .current_dir(&docker_dir)
        .status()
        .await?;

    if !status.success() {
        bail!("Failed to start docker compose");
    }

    let client = wait_for_server_ready(zkboost_server_port).await?;
    Ok(ClientSession {
        client,
        _resources: ServerResources::Docker(guard),
    })
}

/// Start `zkboost-server` directly using the binary or cargo.
async fn start_zkboost_server_raw(
    args: &Args,
    config_path: &str,
    zkboost_server_port: u16,
) -> anyhow::Result<ClientSession> {
    info!("Starting server...");

    // Start the zkboost server
    let mut cmd;
    match args.zkboost_server_bin.as_deref() {
        Some(bin) => cmd = Command::new(bin),
        None => {
            cmd = Command::new("cargo");
            cmd.args(["run", "--release", "--bin", "zkboost-server", "--"]);
        }
    }

    // Set up the child process to receive SIGTERM when parent exits
    unsafe { cmd.pre_exec(|| prctl::set_pdeathsig(Signal::SIGTERM).map_err(io::Error::other)) };

    cmd.env("ERE_IMAGE_REGISTRY", &args.ere_image_registry)
        .args(["--config", config_path])
        .spawn()?;

    let client = wait_for_server_ready(zkboost_server_port).await?;
    Ok(ClientSession {
        client,
        _resources: ServerResources::Raw,
    })
}

/// Wait for the server to be ready by polling the health endpoint.
async fn wait_for_server_ready(port: u16) -> anyhow::Result<zkboostClient> {
    let client = zkboostClient::new(format!("http://localhost:{port}"))?;
    let start = Instant::now();
    loop {
        match client.health().await {
            Ok(_) => break,
            Err(_) if start.elapsed().as_secs() >= 300 => {
                bail!("Server failed to start after 5 mins")
            }
            Err(_) => {
                info!("Waiting for server to be ready...");
                sleep(Duration::from_secs(10)).await
            }
        }
    }

    info!("Successfully started server");

    Ok(client)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    let workspace = tempdir()?;
    let workspace = if args.keep_workspace {
        let path = workspace.keep();
        info!("Keep server workspace at {}", path.display());
        path
    } else {
        workspace.path().to_path_buf()
    };

    let zkboost_server_port = 3001;

    // Webhook url to receive proof results
    let webhook_port = 3003;
    let webhook_url = format!("http://127.0.0.1:{webhook_port}/webhook");

    let config_path = generate_config(&args, &workspace, zkboost_server_port, &webhook_url).await?;

    let client = start_zkboost_server(&args, &config_path, zkboost_server_port).await?;

    let program_id = args.program_id();

    let el_input = ElInput::new(fetch_empty_block(&workspace).await?);
    let stdin = el_input.to_zkvm_input(args.el)?.stdin;

    // Execution

    info!("Requesting execution...");

    let response = client.execute(&program_id, stdin.clone()).await?;

    assert_eq!(response.program_id.0, program_id);
    assert_eq!(
        el_input.expected_public_values(args.el, true)?.as_slice(),
        response.public_values.as_slice(),
    );
    info!(
        "Execution time: {:?}",
        Duration::from_millis(response.execution_time_ms as u64)
    );

    if args.skip_prove {
        info!("Skip proving");
        return Ok(());
    }

    // Proving

    // Create channel for receiving proof results
    let (proof_tx, mut proof_rx) = mpsc::channel::<ProofResult>(1);

    let webhook_app = Router::new()
        .route("/webhook", post(webhook_handler))
        .with_state(proof_tx)
        .layer(TraceLayer::new_for_http());

    let webhook_addr = format!("127.0.0.1:{webhook_port}");
    let listener = tokio::net::TcpListener::bind(&webhook_addr).await?;
    info!("Webhook server listening on {webhook_addr}");

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, webhook_app).await {
            tracing::error!(error = %e, "Webhook server error");
        }
    });

    info!("Requesting proving...");

    // Submit prove request
    let response = client.prove(&program_id, stdin).await?;
    let proof_gen_id = response.proof_gen_id.clone();

    info!("Proof request submitted");
    info!("Waiting for proof {proof_gen_id} via webhook...");

    // Wait for the webhook to receive the proof result
    let (proof, public_values, proving_time_ms) = match proof_rx.recv().await {
        Some(proof_result) => {
            if proof_result.proof_gen_id == proof_gen_id {
                if let Some(error) = &proof_result.error {
                    bail!("Proof generation failed: {}", error);
                }

                info!("Proof completed");
                (
                    proof_result.proof,
                    proof_result.public_values,
                    proof_result.proving_time_ms,
                )
            } else {
                bail!("Unexpected proof_gen_id {}", proof_result.proof_gen_id);
            }
        }
        None => {
            bail!("Webhook channel closed unexpectedly");
        }
    };

    assert_eq!(
        el_input.expected_public_values(args.el, true)?.as_slice(),
        public_values.as_slice(),
    );
    info!(
        "Proving time: {:?}, proof size: {} KiB",
        Duration::from_millis(proving_time_ms as u64),
        proof.len() as f64 / 1024f64,
    );

    // Verifying (valid proof)

    info!("Requesting verifying a valid proof...");

    let response = client.verify(&program_id, proof.clone()).await?;

    assert_eq!(response.program_id.0, program_id);
    assert!(response.verified);
    assert_eq!(
        el_input.expected_public_values(args.el, true)?.as_slice(),
        response.public_values.as_slice(),
    );
    info!("Successfully verified");

    Ok(())
}

async fn webhook_handler(
    State(tx): State<mpsc::Sender<ProofResult>>,
    Json(proof_result): Json<ProofResult>,
) -> Result<StatusCode, (StatusCode, String)> {
    info!(
        proof_gen_id = %proof_result.proof_gen_id,
        "Received proof result via webhook"
    );

    // Send the result through the channel
    if tx.send(proof_result).await.is_err() {
        tracing::warn!("Failed to send proof result - receiver dropped");
    }

    Ok(StatusCode::OK)
}

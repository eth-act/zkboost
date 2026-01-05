//! Integration tests for the zkboost-server HTTP API with stateless validator
//! program.

#![allow(unused_crate_dependencies)]

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::bail;
use clap::Parser;
use ere_dockerized::{CRATE_VERSION, zkVMKind};
use ere_zkvm_interface::ProverResourceType;
use nix::sys::{prctl, signal::Signal};
use tempfile::tempdir;
use tokio::{process::Command, time::sleep};
use tracing::info;
use zkboost_client::zkboostClient;
use zkboost_ethereum_el_config::program::download_program;
use zkboost_ethereum_el_input::ElInput;
use zkboost_ethereum_el_types::ElKind;
use zkboost_server_config::{Config, zkVMConfig};

use crate::utils::{ClientSession, DockerComposeGuard, ServerResources};

mod utils;

const EMPTY_BLOCK: &[u8] = include_bytes!("./fixtures/empty_block.json");

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
    fn server_image(&self) -> String {
        format!(
            "{}/ere-server-{}:{CRATE_VERSION}",
            self.ere_image_registry, self.zkvm
        )
    }

    fn program_id(&self) -> String {
        format!("{}-{}", self.el.as_str(), self.zkvm)
    }
}

/// Download program and generate config file.
async fn generate_config(args: &Args, workspace: &Path) -> anyhow::Result<String> {
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
        zkvm: vec![zkVMConfig {
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
async fn start_zkboost_server(args: &Args, config_path: &str) -> anyhow::Result<ClientSession> {
    let port = 3001;

    // Pull server image if using CPU resource
    let server_image = args.server_image();
    if args.resource == "cpu" {
        info!("Pulling image {server_image}...");

        let status = Command::new("docker")
            .args(["pull", &server_image])
            .status()
            .await?;
        if !status.success() {
            bail!("Failed to pull Docker image: {server_image}");
        }
    }

    if args.dockerized {
        start_zkboost_server_dockerized(config_path, port).await
    } else {
        start_zkboost_server_raw(args, config_path, port).await
    }
}

/// Start `zkboost-server` using docker compose.
async fn start_zkboost_server_dockerized(
    config_path: &str,
    port: u16,
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

    let client = wait_for_server_ready(port).await?;
    Ok(ClientSession {
        client,
        _resources: ServerResources::Docker(guard),
    })
}

/// Start `zkboost-server` directly using the binary or cargo.
async fn start_zkboost_server_raw(
    args: &Args,
    config_path: &str,
    port: u16,
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
        .args(["--config", config_path, "--port", &port.to_string()])
        .spawn()?;

    let client = wait_for_server_ready(port).await?;
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

    let config_path = generate_config(&args, &workspace).await?;

    let client = start_zkboost_server(&args, &config_path).await?;

    let program_id = args.program_id();

    let el_input = ElInput::new(serde_json::from_slice(EMPTY_BLOCK)?);
    let stdin = el_input.to_zkvm_input(args.el)?.stdin;

    // Execution

    info!("Requesting execution...");

    let response = client.execute(&program_id, stdin.clone()).await?;

    assert_eq!(response.program_id.0, program_id);
    assert_eq!(
        el_input.output_sha256(args.el, true)?.as_slice(),
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

    info!("Requesting proving...");

    let response = client.prove(&program_id, stdin).await?;

    assert_eq!(response.program_id.0, program_id);
    assert_eq!(
        el_input.output_sha256(args.el, true)?.as_slice(),
        response.public_values.as_slice(),
    );
    info!(
        "Proving time: {:?}, proof size: {} KiB",
        Duration::from_millis(response.proving_time_ms as u64),
        response.proof.len() as f64 / 1024f64,
    );

    // Verifying (valid proof)

    info!("Requesting verifying a valid proof...");

    let proof = response.proof;
    let response = client.verify(&program_id, proof.clone()).await?;

    assert_eq!(response.program_id.0, program_id);
    assert!(response.verified);
    assert_eq!(
        el_input.output_sha256(args.el, true)?.as_slice(),
        response.public_values.as_slice(),
    );
    info!("Successfully verified");

    // Verifying (invalid proof)

    info!("Requesting verifying an invalid proof...");

    let mut proof = proof;
    *proof.last_mut().unwrap() ^= 1;
    let response = client.verify(&program_id, proof).await?;

    assert_eq!(response.program_id.0, program_id);
    assert!(!response.verified);
    info!(
        "Verification failed as expected, reason: {}",
        response.failure_reason
    );

    Ok(())
}

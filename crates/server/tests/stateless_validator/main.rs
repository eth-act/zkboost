//! Integration tests for the zkboost-server HTTP API with stateless validator
//! program.

#![allow(unused_crate_dependencies)]

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::bail;
use benchmark_runner::guest_programs::{GuestFixture, OutputVerifierResult};
use clap::{Parser, builder::PossibleValuesParser};
use ere_dockerized::{CRATE_VERSION, zkVMKind};
use nix::sys::{prctl, signal::Signal};
use tempfile::tempdir;
use tokio::{process::Command, time::sleep};
use tracing::info;
use zkboost_client::zkboostClient;

use crate::{config::generate_config, witness::generate_stateless_validator_fixture};

mod config;
mod util;
mod witness;

#[derive(Parser)]
struct Args {
    /// Path to `zkboost-server` binary.
    /// If not provided, `cargo run --release --bin zkboost-server` will be used.
    #[arg(long)]
    zkboost_server_bin: Option<PathBuf>,
    /// Execution layer client implementation (reth or ethrex)
    #[arg(long, default_value = "reth", ignore_case = true, value_parser = PossibleValuesParser::new(["reth", "ethrex"]))]
    el: String,
    /// zkVM to use
    #[arg(long)]
    zkvm: zkVMKind,
    /// Resource type for proving (cpu or gpu)
    #[arg(long, default_value = "cpu", ignore_case = true, value_parser = PossibleValuesParser::new(["cpu", "gpu"]))]
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
    /// Required when `benchmark-runner` dependency uses a git revision instead of a released tag.
    #[arg(env = "GITHUB_TOKEN")]
    github_token: Option<String>,
}

impl Args {
    fn server_image(&self) -> String {
        format!(
            "{}/ere-server-{}:{CRATE_VERSION}",
            self.ere_image_registry, self.zkvm
        )
    }

    fn program_id(&self) -> String {
        format!("{}-{}", self.el, self.zkvm)
    }
}

/// Start `zkboost-server` with stateless validator program.
async fn start_zkboost_server(args: &Args, workspace: &Path) -> anyhow::Result<zkboostClient> {
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

    // Create the config
    let config_path = generate_config(args, workspace).await?;

    info!("Starting server...");

    // Start the zkboost server
    let port = 3001;
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
        .args(["--config", &config_path, "--port", &port.to_string()])
        .spawn()?;

    // Wait for the server to be ready
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

    let client = start_zkboost_server(&args, &workspace).await?;

    let fixture = generate_stateless_validator_fixture(&args, &workspace).await?;

    let program_id = args.program_id();
    let stdin = fixture.input()?.stdin;

    // Execution

    info!("Requesting execution...");

    let response = client.execute(&program_id, stdin.clone()).await?;

    assert_eq!(response.program_id.0, program_id);
    assert!(matches!(
        fixture.verify_public_values(&response.public_values)?,
        OutputVerifierResult::Match
    ));
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
    assert!(matches!(
        fixture.verify_public_values(&response.public_values),
        Ok(OutputVerifierResult::Match)
    ));
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
    assert!(matches!(
        fixture.verify_public_values(&response.public_values),
        Ok(OutputVerifierResult::Match)
    ));
    info!("Successfully verified");

    // Verifying (invalid proof)

    info!("Requesting verifying an invalid proof...");

    let mut proof = proof;
    *proof.last_mut().unwrap() ^= 1;
    let response = client.verify(&program_id, proof).await?;

    assert_eq!(response.program_id.0, program_id);
    assert!(!response.verified);
    assert!(matches!(
        fixture.verify_public_values(&response.public_values),
        Ok(OutputVerifierResult::Mismatch(_))
    ));
    info!("Verification failed as expected");

    Ok(())
}

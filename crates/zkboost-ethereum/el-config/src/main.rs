//! Configuration file generator for zkboost-server Ethereum Execution Layer
//! stateless validator.
//!
//! This binary generates configuration files that enable zkboost-server to
//! serve Ethereum Execution Layer stateless validator guest programs.

use std::path::PathBuf;

use clap::Parser;
use ere_common::zkVMKind;
use ere_zkvm_interface::ProverResource;
use tokio::fs;
use zkboost_ethereum_el_config::program::download_program;
use zkboost_ethereum_el_types::ElKind;
use zkboost_server_config::{Config, zkVMConfig};

#[derive(Parser)]
struct Args {
    /// Execution layer client implementation (reth or ethrex)
    #[arg(long)]
    el: ElKind,
    /// zkVM to use
    #[arg(long)]
    zkvm: zkVMKind,
    /// Resource type for proving (cpu or gpu)
    #[arg(long, ignore_case = true, default_value = "cpu", value_parser = ["cpu", "gpu"])]
    resource: String,
    /// Output path to save the `config.toml` and the program.
    #[arg(long)]
    output_dir: PathBuf,
    /// Download the artifact even if a release url is available
    #[arg(long)]
    download_guest: bool,
    /// GitHub token for downloading artifacts from GitHub Actions.
    /// Required when `ere-guests` dependency uses a git revision instead of a released tag.
    #[arg(env = "GITHUB_TOKEN")]
    github_token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    let program = download_program(
        args.el,
        args.zkvm,
        args.github_token.as_deref(),
        &args.output_dir,
        args.download_guest,
    )
    .await?;

    let config = Config {
        port: 3001,
        webhook_url: "http://localhost:3003/proofs".to_string(),
        zkvm: vec![zkVMConfig::Docker {
            kind: args.zkvm,
            resource: match args.resource.to_lowercase().as_str() {
                "cpu" => ProverResource::Cpu,
                "gpu" => ProverResource::Gpu,
                _ => unreachable!(),
            },
            program_id: format!("{}-{}", args.el, args.zkvm).into(),
            program,
        }],
    };

    fs::write(args.output_dir.join("config.toml"), config.to_toml()?).await?;

    Ok(())
}

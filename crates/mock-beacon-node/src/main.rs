//! The mock beacon node of the examples.

use std::{fs, net::Ipv4Addr, path::PathBuf, sync::Arc};

use anyhow::Context;
use clap::Parser;
use mock_beacon_node::{DEFAULT_PROOF_ENGINE_JSON, MockBeaconNode};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Parser)]
struct Cli {
    /// Beacon API endpoint of the CL whose Engine API requests the mock forwards.
    #[arg(long)]
    cl_endpoint: Url,
    /// Engine API endpoint of zkboost.
    #[arg(long)]
    zkboost_endpoint: Url,
    /// Port serving the beacon API.
    #[arg(long, default_value_t = 3001)]
    port: u16,
    /// Port serving the Engine API to the CL.
    #[arg(long, default_value_t = 8551)]
    engine_port: u16,
    /// `proof-engine.json` of the eth-act Lighthouse with the verifying keys of the proof types
    /// to verify, by default the embedded one with every proof type.
    #[arg(long)]
    proof_engine: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let proof_engine_json = match &cli.proof_engine {
        Some(path) => {
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
        }
        None => DEFAULT_PROOF_ENGINE_JSON.to_owned(),
    };
    let node = MockBeaconNode::new(cli.cl_endpoint, cli.zkboost_endpoint, &proof_engine_json)?;
    let beacon_api = TcpListener::bind((Ipv4Addr::UNSPECIFIED, cli.port)).await?;
    let engine_api = TcpListener::bind((Ipv4Addr::UNSPECIFIED, cli.engine_port)).await?;
    Ok(Arc::new(node).serve(beacon_api, engine_api).await?)
}

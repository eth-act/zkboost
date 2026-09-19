//! Mock beacon node. It mocks a beacon node with the EIP-8025 behavior. It serves the Engine API
//! to a CL and forwards every request to zkboost, so zkboost sees the Engine API traffic of a
//! real CL. It receives the signed EIP-8025 envelopes at `POST /eth/v1/beacon/execution_proofs`
//! and verifies the signature and the proof. Every other beacon API request goes to the CL.

#![warn(unused_crate_dependencies)]

use std::{net::Ipv4Addr, sync::Arc};

use clap::Parser;
use mock_beacon_node::MockBeaconNode;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use url::Url;
use zkboost_types::ProofType;

mod beacon_node_client;
mod engine_api_client;
mod mock_beacon_node;

#[derive(Parser)]
struct Cli {
    /// Beacon API endpoint of the CL whose Engine API requests the mock forwards.
    #[arg(long)]
    cl_endpoint: Url,
    /// Engine API endpoint of zkboost.
    #[arg(long)]
    zkboost_endpoint: Url,
    /// Proof types expected for every payload.
    #[arg(long, value_delimiter = ',')]
    proof_types: Vec<ProofType>,
    /// Port serving the beacon API.
    #[arg(long, default_value_t = 3001)]
    port: u16,
    /// Port serving the Engine API to the CL.
    #[arg(long, default_value_t = 8551)]
    engine_port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let mock_beacon_node = Arc::new(
        MockBeaconNode::new(cli.cl_endpoint, cli.zkboost_endpoint, &cli.proof_types).await?,
    );
    let beacon_api = TcpListener::bind((Ipv4Addr::UNSPECIFIED, cli.port)).await?;
    let engine_api = TcpListener::bind((Ipv4Addr::UNSPECIFIED, cli.engine_port)).await?;
    tokio::try_join!(
        mock_beacon_node.clone().serve_beacon_api(beacon_api),
        mock_beacon_node.serve_engine_api(engine_api),
    )?;
    Ok(())
}

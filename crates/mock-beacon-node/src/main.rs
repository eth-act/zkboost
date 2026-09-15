//! Mock beacon node. It mocks a beacon node with the EIP-8025 behavior. It follows the beacon
//! chain head of a CL, sends every Gloas payload to zkboost as `engine_newPayloadV5`, receives the
//! signed EIP-8025 envelopes at `POST /eth/v1/beacon/execution_proofs`, and verifies the signature
//! and the proof. Every other beacon API request goes to the CL.

#![warn(unused_crate_dependencies)]

use std::sync::Arc;

use anyhow::bail;
use clap::Parser;
use jsonwebtoken as _;
use mock_beacon_node::MockBeaconNode;
use tokio_stream::StreamExt;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;
use zkboost_types::ProofType;

mod beacon_node_client;
mod mock_beacon_node;

#[derive(Parser)]
struct Cli {
    /// Beacon API endpoint of the CL to follow.
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

    {
        let mock_beacon_node = mock_beacon_node.clone();
        tokio::spawn(async move { mock_beacon_node.serve(cli.port).await });
    }

    let mut stream = mock_beacon_node.beacon_node_client.subscribe_blocks();
    while let Some(block) = stream.next().await {
        info!(slot = %block.slot, block = %block.block, "new block");
        let mock_beacon_node = mock_beacon_node.clone();
        tokio::spawn(async move {
            if let Err(error) = mock_beacon_node.process_block(block.block).await {
                warn!(slot = %block.slot, block = %block.block, error = %error, "block failed");
            }
        });
    }
    bail!("block stream ended")
}

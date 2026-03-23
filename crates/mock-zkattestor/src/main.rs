//! Mock zkattestor.

#![warn(unused_crate_dependencies)]

use std::{collections::HashSet, sync::Arc};

use anyhow::bail;
use cl_client::{ClClient, new_payload_request_from_beacon_block};
use clap::Parser;
use futures::StreamExt;
use lighthouse_types::Hash256;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;
use zkboost_client::zkBoostClient;
use zkboost_types::{ProofEvent, ProofType};

mod cl_client;

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    cl_endpoint: Url,
    #[arg(long)]
    zkboost_endpoint: Url,
    #[arg(long, value_delimiter = ',')]
    proof_types: Vec<ProofType>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let mock_attestor = Arc::new(MockAttestor {
        cl_client: ClClient::new(cli.cl_endpoint),
        zkboost_client: zkBoostClient::new(cli.zkboost_endpoint),
        proof_types: cli.proof_types,
    });

    let mut stream = Box::pin(mock_attestor.cl_client.subscribe_head_events());
    while let Some(Ok(head)) = stream.next().await {
        info!(slot = head.slot, block = %head.block, "new head");
        let mock_attestor = mock_attestor.clone();
        tokio::spawn(async move {
            if let Err(error) = mock_attestor.process_slot(head.slot).await {
                warn!(slot = head.slot, error = %error, "slot failed");
            }
        });
    }
    bail!("head stream ended")
}

struct MockAttestor {
    cl_client: ClClient,
    zkboost_client: zkBoostClient,
    proof_types: Vec<ProofType>,
}

impl MockAttestor {
    async fn process_slot(&self, slot: u64) -> anyhow::Result<()> {
        let beacon_block = self.cl_client.get_beacon_block(slot).await?;
        let new_payload_request = new_payload_request_from_beacon_block(&beacon_block)?;

        let block_hash = new_payload_request.block_hash();
        let resp = self
            .zkboost_client
            .request_proof(&new_payload_request, &self.proof_types)
            .await?;
        let new_payload_request_root = resp.new_payload_request_root;
        info!(%new_payload_request_root, %block_hash, "proof requested");

        let mut proof_events = Box::pin(
            self.zkboost_client
                .subscribe_proof_events(Some(new_payload_request_root)),
        );
        let mut remaining: HashSet<ProofType> = self.proof_types.iter().copied().collect();

        while !remaining.is_empty() {
            let Some(Ok(proof_event)) = proof_events.next().await else {
                bail!("proof stream ended");
            };

            remaining.remove(&proof_event.proof_type());

            match proof_event {
                ProofEvent::ProofComplete(proof_complete) => {
                    info!(%new_payload_request_root, proof_type = %proof_complete.proof_type, "proof complete");
                    match self
                        .download_and_verify(new_payload_request_root, proof_complete.proof_type)
                        .await
                    {
                        Ok(()) => {
                            info!(%new_payload_request_root, proof_type = %proof_complete.proof_type, "proof verified")
                        }
                        Err(e) => {
                            warn!(%new_payload_request_root, proof_type = %proof_complete.proof_type, error = %e, "proof verification failed")
                        }
                    }
                }
                ProofEvent::ProofFailure(proof_failure) => {
                    warn!(
                        %new_payload_request_root,
                        proof_type = %proof_failure.proof_type,
                        reason = ?proof_failure.reason,
                        error = %proof_failure.error,
                        "proof failed"
                    )
                }
            }
        }

        info!(%new_payload_request_root, "all proofs done");

        Ok(())
    }

    async fn download_and_verify(
        &self,
        new_payload_request_root: Hash256,
        proof_type: ProofType,
    ) -> anyhow::Result<()> {
        let proof = self
            .zkboost_client
            .get_proof(new_payload_request_root, proof_type)
            .await?;
        let response = self
            .zkboost_client
            .verify_proof(new_payload_request_root, proof_type, &proof)
            .await?;
        if !response.status.is_valid() {
            anyhow::bail!("invalid proof");
        }
        Ok(())
    }
}

//! Delivery of generated proofs to a beacon node as validator-signed EIP-8025 execution proof
//! envelopes. A proof is submitted under every beacon block that carries the proven payload and
//! signed under the fork at the slot of the block, computed from the chain spec of the beacon node
//! as a validator client does.

use std::fs;

use alloy_primitives::B256;
use anyhow::{Context, anyhow, bail, ensure};
use lighthouse_bls::Keypair;
use lighthouse_eth2_keystore::Keystore;
use lighthouse_types::{ChainSpec, Config as SpecConfig, EthSpec, MainnetEthSpec, Slot};
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, de::DeserializeOwned};
use tokio::sync::OnceCell;
use tracing::info;
use url::Url;
use zkboost_types::{
    ExecutionProofEnvelope, SignedExecutionProofEnvelope, SignedExecutionProofEnvelopes, SszEncode,
    SszList, SszVector, execution_proof_domain,
};

use crate::config::Config;

/// Signs every generated proof as a validator and posts it to the beacon node.
#[allow(missing_debug_implementations)]
pub(crate) struct ProofSubmitter {
    beacon_endpoint: Url,
    keypair: Keypair,
    http_client: reqwest::Client,
    spec: OnceCell<ChainSpec>,
    validator_index: OnceCell<u64>,
    genesis_validators_root: OnceCell<B256>,
}

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

#[derive(Deserialize)]
struct Genesis {
    genesis_validators_root: B256,
}

#[derive(Deserialize)]
struct Validator {
    #[serde(with = "serde_utils::quoted_u64")]
    index: u64,
}

#[derive(Deserialize)]
struct BlockHeader {
    root: B256,
    header: SignedHeader,
}

#[derive(Deserialize)]
struct SignedHeader {
    message: HeaderMessage,
}

#[derive(Deserialize)]
struct HeaderMessage {
    slot: Slot,
}

#[derive(Deserialize)]
struct SignedBlock {
    message: Block,
}

#[derive(Deserialize)]
struct Block {
    body: BlockBody,
}

#[derive(Deserialize)]
struct BlockBody {
    signed_execution_payload_bid: SignedBid,
}

#[derive(Deserialize)]
struct SignedBid {
    message: Bid,
}

#[derive(Deserialize)]
struct Bid {
    block_hash: B256,
}

impl ProofSubmitter {
    /// Creates a submitter that signs with the key of the configured keystore.
    pub(crate) fn new(config: &Config) -> anyhow::Result<Self> {
        let keystore =
            Keystore::from_json_file(&config.validator_keystore_path).map_err(|error| {
                anyhow!(
                    "read keystore {}: {error:?}",
                    config.validator_keystore_path.display()
                )
            })?;
        let password = fs::read(&config.validator_keystore_password_path).with_context(|| {
            format!(
                "read password {}",
                config.validator_keystore_password_path.display()
            )
        })?;
        // The trailing newlines of the password file are not part of the password, as in
        // lighthouse.
        let password_end = password
            .iter()
            .rposition(|byte| !matches!(byte, b'\n' | b'\r'))
            .map_or(0, |position| position + 1);
        let keypair = keystore
            .decrypt_keypair(&password[..password_end])
            .map_err(|error| anyhow!("decrypt keystore: {error:?}"))?;
        info!(pubkey = %keypair.pk, "validator keystore decrypted");
        Ok(Self {
            beacon_endpoint: config.cl_beacon_endpoint.clone(),
            keypair,
            http_client: reqwest::Client::new(),
            spec: OnceCell::new(),
            validator_index: OnceCell::new(),
            genesis_validators_root: OnceCell::new(),
        })
    }

    /// Signs and submits a proof of a payload under every beacon block that carries the payload, or
    /// errors when the beacon node does not accept it.
    pub(crate) async fn submit(
        &self,
        block_hash: B256,
        parent_beacon_block_root: B256,
        proof_type: u8,
        proof_data: Vec<u8>,
    ) -> anyhow::Result<()> {
        let blocks = self
            .find_blocks(block_hash, parent_beacon_block_root)
            .await?;
        let spec = self.spec().await?;
        let genesis_validators_root = self.genesis_validators_root().await?;
        let validator_index = self.validator_index().await?;
        let proof_data = SszList::try_from(proof_data)
            .map_err(|error| anyhow!("proof exceeds MAX_PROOF_SIZE: {error:?}"))?;
        for (beacon_block_root, slot) in blocks {
            let fork = spec.fork_at_epoch(slot.epoch(MainnetEthSpec::slots_per_epoch()));
            let domain = execution_proof_domain(fork.current_version, genesis_validators_root);
            let message = ExecutionProofEnvelope {
                proof_data: proof_data.clone(),
                proof_type,
                beacon_block_root: beacon_block_root.0,
            };
            let signing_root = message.signing_root(domain);
            let signature = self
                .keypair
                .sk
                .sign(lighthouse_bls::Hash256::from_slice(signing_root.as_slice()));
            let envelope = SignedExecutionProofEnvelope {
                message,
                validator_index,
                signature: SszVector::try_from(signature.serialize().to_vec())
                    .expect("a BLS signature has 96 bytes"),
            };
            let envelopes: SignedExecutionProofEnvelopes =
                SszList::try_from(vec![envelope]).expect("one envelope is within the bound");
            let mut body = Vec::new();
            envelopes.ssz_append(&mut body);
            self.post("eth/v1/beacon/execution_proofs", body).await?;
        }
        Ok(())
    }

    /// Returns the chain spec of the beacon node, read on the first call.
    async fn spec(&self) -> anyhow::Result<&ChainSpec> {
        self.spec
            .get_or_try_init(|| async {
                let config: SpecConfig = self
                    .get("eth/v1/config/spec")
                    .await?
                    .context("spec not found")?;
                ChainSpec::from_config::<MainnetEthSpec>(&config)
                    .context("beacon node spec is not the mainnet preset")
            })
            .await
    }

    /// Returns the index of the signing validator, read from the head state on the first call.
    async fn validator_index(&self) -> anyhow::Result<u64> {
        self.validator_index
            .get_or_try_init(|| async {
                let path = format!("eth/v1/beacon/states/head/validators/{}", self.keypair.pk);
                let validator: Validator = self
                    .get(&path)
                    .await?
                    .context("signing validator not found")?;
                Ok(validator.index)
            })
            .await
            .copied()
    }

    /// Returns the genesis validators root of the beacon node, read on the first call.
    async fn genesis_validators_root(&self) -> anyhow::Result<B256> {
        self.genesis_validators_root
            .get_or_try_init(|| async {
                let genesis: Genesis = self
                    .get("eth/v1/beacon/genesis")
                    .await?
                    .context("genesis not found")?;
                Ok(genesis.genesis_validators_root)
            })
            .await
            .copied()
    }

    /// Returns the root and the slot of every beacon block under the parent root whose payload bid
    /// carries the payload.
    async fn find_blocks(
        &self,
        block_hash: B256,
        parent_beacon_block_root: B256,
    ) -> anyhow::Result<Vec<(B256, Slot)>> {
        let path = format!("eth/v1/beacon/headers?parent_root={parent_beacon_block_root}");
        let headers: Vec<BlockHeader> = self.get(&path).await?.unwrap_or_default();
        let mut blocks = Vec::new();
        for header in headers {
            let path = format!("eth/v2/beacon/blocks/{}", header.root);
            let block: SignedBlock = self
                .get(&path)
                .await?
                .with_context(|| format!("beacon block {} not found", header.root))?;
            if block
                .message
                .body
                .signed_execution_payload_bid
                .message
                .block_hash
                == block_hash
            {
                blocks.push((header.root, header.header.message.slot));
            }
        }
        ensure!(
            !blocks.is_empty(),
            "no beacon block with payload {block_hash} under {parent_beacon_block_root}"
        );
        Ok(blocks)
    }

    /// Posts an SSZ body to a beacon API route, or errors with the answer of the beacon node.
    async fn post(&self, path: &str, body: Vec<u8>) -> anyhow::Result<()> {
        let url = self.beacon_endpoint.join(path)?;
        let response = self
            .http_client
            .post(url.clone())
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("POST {url}: {status}: {body}");
        }
        Ok(())
    }

    /// Fetches the `data` of a beacon API resource, or `None` when it does not exist.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<Option<T>> {
        let url = self.beacon_endpoint.join(path)?;
        let response = self
            .http_client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("GET {url}: {status}: {body}");
        }
        Ok(Some(response.json::<Data<T>>().await?.data))
    }
}

//! Client of the beacon API. It reads the chain spec, the genesis validators root, and the
//! validator index. It posts the signed envelopes and resolves the beacon block that carries a
//! payload. The `execution_payload` event stream reports that block root while the proof is still
//! running.

use std::time::Duration;

use alloy_primitives::B256;
use anyhow::{Context, anyhow, bail, ensure};
use lighthouse_bls::PublicKey;
use lighthouse_types::{BeaconState, ChainSpec, ForkName, MainnetEthSpec, Slot};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest_eventsource::{Event, EventSource, retry::Constant};
use serde::{Deserialize, de::DeserializeOwned};
use tokio_stream::{Stream, StreamExt};
use tracing::warn;
use url::Url;
use zkboost_types::{BeaconStateWitness, SignedExecutionProofEnvelopes, SszEncode};

/// Timeout of a beacon node request, the slot time of mainnet.
const BEACON_NODE_TIMEOUT: Duration = Duration::from_secs(12);
/// Delay between two attempts of a beacon node request.
pub(crate) const BEACON_NODE_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Client of the beacon API of the configured endpoint.
#[derive(Debug)]
pub(crate) struct BeaconNodeClient {
    endpoint: Url,
    http_client: reqwest::Client,
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
struct ValidatorData {
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
    state_root: B256,
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

/// An `execution_payload` event of the beacon API event stream.
#[derive(Debug, Deserialize)]
pub(crate) struct ExecutionPayloadEvent {
    /// Slot of the payload.
    pub(crate) slot: Slot,
    /// Block hash of the payload.
    pub(crate) block_hash: B256,
    /// Root of the beacon block that carries the payload.
    pub(crate) block_root: B256,
}

impl BeaconNodeClient {
    /// Creates the client of the beacon API at the endpoint.
    pub(crate) fn new(endpoint: Url) -> anyhow::Result<Self> {
        Ok(Self {
            endpoint,
            http_client: reqwest::Client::builder()
                .timeout(BEACON_NODE_TIMEOUT)
                .build()?,
        })
    }

    /// Subscribes to the `execution_payload` events of the beacon node. The stream reconnects
    /// after a lost connection and does not end.
    pub(crate) fn subscribe_execution_payloads(
        &self,
    ) -> impl Stream<Item = ExecutionPayloadEvent> + '_ {
        let mut url = self
            .endpoint
            .join("eth/v1/events")
            .expect("the beacon endpoint accepts a relative path");
        url.query_pairs_mut()
            .append_pair("topics", "execution_payload");
        // A request timeout covers the whole response, so the stream needs a client without one.
        let mut event_source = EventSource::new(reqwest::Client::new().get(url))
            .expect("the stream request carries no body");
        event_source.set_retry_policy(Box::new(Constant::new(BEACON_NODE_RETRY_DELAY, None)));
        event_source.filter_map(|event| match event {
            Ok(Event::Message(message)) if message.event == "execution_payload" => {
                serde_json::from_str::<ExecutionPayloadEvent>(&message.data)
                    .inspect_err(|error| warn!(%error, "execution payload event not decodable"))
                    .ok()
            }
            Ok(_) => None,
            Err(error) => {
                warn!(error = %format!("{error:#}"), "execution payload stream lost");
                None
            }
        })
    }

    /// Returns the beacon block root of the payload from the children of the parent root. Exactly
    /// one child must carry the payload at the slot of the payload.
    pub(crate) async fn beacon_block_root(
        &self,
        block_hash: B256,
        parent_beacon_block_root: B256,
        slot: Slot,
    ) -> anyhow::Result<B256> {
        let mut blocks = Vec::new();
        for header in self.headers_of_parent(parent_beacon_block_root).await? {
            let block = self.block(header.root).await?;
            if block
                .message
                .body
                .signed_execution_payload_bid
                .message
                .block_hash
                == block_hash
            {
                let block_slot = header.header.message.slot;
                ensure!(
                    block_slot == slot,
                    "beacon block {} has slot {block_slot}, payload has slot {slot}",
                    header.root
                );
                blocks.push(header.root);
            }
        }
        match blocks.as_slice() {
            [block] => Ok(*block),
            [] => {
                bail!("no beacon block with payload {block_hash} under {parent_beacon_block_root}")
            }
            _ => bail!(
                "ambiguous payload {block_hash} under {parent_beacon_block_root}, carried by {} beacon blocks",
                blocks.len()
            ),
        }
    }

    /// Lists the headers of the children of a beacon block root.
    async fn headers_of_parent(
        &self,
        parent_beacon_block_root: B256,
    ) -> anyhow::Result<Vec<BlockHeader>> {
        let path = format!("eth/v1/beacon/headers?parent_root={parent_beacon_block_root}");
        Ok(self.get(&path).await?.unwrap_or_default())
    }

    /// Returns the signed beacon block of a root.
    async fn block(&self, root: B256) -> anyhow::Result<SignedBlock> {
        let path = format!("eth/v2/beacon/blocks/{root}");
        self.get(&path)
            .await?
            .with_context(|| format!("beacon block {root} not found"))
    }

    /// Returns the state root of a beacon block.
    pub(crate) async fn state_root(&self, block_root: B256) -> anyhow::Result<B256> {
        Ok(self.block(block_root).await?.message.state_root)
    }

    /// Returns the leaves of a post-Gloas beacon state, which must merkleize to its root.
    pub(crate) async fn beacon_state_witness(
        &self,
        state_root: B256,
        spec: &ChainSpec,
    ) -> anyhow::Result<BeaconStateWitness> {
        let path = format!("eth/v2/debug/beacon/states/{state_root}");
        let bytes = self
            .get_ssz(&path)
            .await?
            .with_context(|| format!("beacon state {state_root} not found"))?;
        let spec = spec.clone();
        tokio::task::spawn_blocking(move || {
            let state = BeaconState::<MainnetEthSpec>::from_ssz_bytes(&bytes, &spec)
                .map_err(|error| anyhow!("beacon state {state_root} not decodable: {error:?}"))?;
            let fork_name = state.fork_name_unchecked();
            ensure!(
                fork_name >= ForkName::Gloas,
                "beacon state {state_root} is under {fork_name}, which has no payload bid"
            );
            let witness = BeaconStateWitness::new(
                state
                    .get_beacon_state_leaves()
                    .into_iter()
                    .map(|leaf| leaf.0)
                    .collect(),
            )?;
            let merkleized = witness.state_root();
            ensure!(
                merkleized == state_root,
                "beacon state leaves merkleize to {merkleized}, not to {state_root}"
            );
            anyhow::Ok(witness)
        })
        .await
        .expect("beacon state decoding does not panic")
    }

    /// Returns the chain spec of the beacon node.
    pub(crate) async fn spec(&self) -> anyhow::Result<ChainSpec> {
        let config = self
            .get("eth/v1/config/spec")
            .await?
            .context("spec not found")?;
        ChainSpec::from_config::<MainnetEthSpec>(&config)
            .context("beacon node spec is not the mainnet preset")
    }

    /// Returns the index of the signing validator in the head state.
    pub(crate) async fn validator_index(&self, pubkey: &PublicKey) -> anyhow::Result<u64> {
        let path = format!("eth/v1/beacon/states/head/validators/{pubkey}");
        let validator: ValidatorData = self
            .get(&path)
            .await?
            .context("signing validator not found")?;
        Ok(validator.index)
    }

    /// Returns the genesis validators root of the beacon node.
    pub(crate) async fn genesis_validators_root(&self) -> anyhow::Result<B256> {
        let genesis: Genesis = self
            .get("eth/v1/beacon/genesis")
            .await?
            .context("genesis not found")?;
        Ok(genesis.genesis_validators_root)
    }

    /// Posts signed execution proof envelopes to the beacon node.
    pub(crate) async fn post_execution_proofs(
        &self,
        envelopes: &SignedExecutionProofEnvelopes,
    ) -> anyhow::Result<()> {
        let mut body = Vec::new();
        envelopes.ssz_append(&mut body);
        self.post("eth/v1/beacon/execution_proofs", body).await
    }

    /// Posts an SSZ body to a beacon API route, or errors with the answer of the beacon node.
    async fn post(&self, path: &str, body: Vec<u8>) -> anyhow::Result<()> {
        let url = self.endpoint.join(path)?;
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
        let url = self.endpoint.join(path)?;
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

    /// Fetches the SSZ body of a beacon API resource, or `None` when it does not exist.
    async fn get_ssz(&self, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let url = self.endpoint.join(path)?;
        let response = self
            .http_client
            .get(url.clone())
            .header(ACCEPT, "application/octet-stream")
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
        Ok(Some(response.bytes().await?.into()))
    }
}

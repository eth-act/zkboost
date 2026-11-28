//! The client of the beacon API. It reads what a proof needs and posts the signed envelopes.

use std::time::Duration;

use alloy_primitives::B256;
use anyhow::{Context, bail};
use lighthouse_types::{ChainSpec, MainnetEthSpec};
use reqwest::{StatusCode, header::CONTENT_TYPE};
use reqwest_eventsource::{Event, EventSource, retry::Constant};
use serde::{Deserialize, de::DeserializeOwned};
use tokio_stream::{Stream, StreamExt};
use tracing::warn;
use url::Url;

use crate::util::{RETRY_DELAY, error_for_status};

/// Timeout of a beacon node request, the slot time of mainnet.
const TIMEOUT: Duration = Duration::from_secs(12);

/// An `execution_payload` event, which reports the beacon block of an imported payload.
#[derive(Debug, Deserialize)]
pub(crate) struct ExecutionPayloadEvent {
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) slot: u64,
    pub(crate) block_hash: B256,
    pub(crate) block_root: B256,
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
struct Header {
    root: B256,
}

#[derive(Deserialize)]
struct Message<T> {
    message: T,
}

#[derive(Deserialize)]
struct Block {
    body: BlockBody,
}

#[derive(Deserialize)]
struct BlockBody {
    signed_execution_payload_bid: Message<Bid>,
}

#[derive(Deserialize)]
struct Bid {
    block_hash: B256,
}

#[derive(Debug)]
pub struct BeaconClient {
    endpoint: Url,
    http: reqwest::Client,
}

impl BeaconClient {
    pub fn new(endpoint: Url) -> Self {
        let http = reqwest::Client::builder().timeout(TIMEOUT).build();
        Self {
            endpoint,
            http: http.expect("the client has a static configuration"),
        }
    }

    /// Fetches the `data` of a resource, or `None` if it does not exist.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<Option<T>> {
        let url = self.endpoint.join(path)?;
        let response = self.http.get(url.clone()).send().await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = error_for_status(response).await;
        let response = response.with_context(|| format!("GET {url}"))?;
        Ok(Some(response.json::<Data<T>>().await?.data))
    }

    pub async fn spec(&self) -> anyhow::Result<ChainSpec> {
        let config = self.get("eth/v1/config/spec").await?.context("no spec")?;
        ChainSpec::from_config::<MainnetEthSpec>(&config)
            .context("beacon node spec is not the mainnet preset")
    }

    pub async fn genesis_validators_root(&self) -> anyhow::Result<B256> {
        let genesis: Genesis = self
            .get("eth/v1/beacon/genesis")
            .await?
            .context("no genesis")?;
        Ok(genesis.genesis_validators_root)
    }

    pub async fn bid_block_hash(&self, block_root: B256) -> anyhow::Result<B256> {
        let block: Message<Block> = self
            .get(&format!("eth/v2/beacon/blocks/{block_root}"))
            .await?
            .with_context(|| format!("no beacon block {block_root}"))?;
        Ok(block
            .message
            .body
            .signed_execution_payload_bid
            .message
            .block_hash)
    }

    /// Returns the root of the beacon block that carries a payload. The block hash of a payload
    /// covers its slot and its parent beacon block root, so the block is a child of that parent
    /// at that slot whose bid names the block hash.
    pub(crate) async fn beacon_block_root(
        &self,
        block_hash: B256,
        parent_beacon_block_root: B256,
        slot: u64,
    ) -> anyhow::Result<B256> {
        let parent_root = parent_beacon_block_root;
        let path = format!("eth/v1/beacon/headers?slot={slot}&parent_root={parent_root}");
        let headers: Vec<Header> = self.get(&path).await?.unwrap_or_default();
        let mut block_roots = Vec::new();
        for Header { root } in headers {
            if self.bid_block_hash(root).await? == block_hash {
                block_roots.push(root);
            }
        }
        match block_roots.as_slice() {
            [block_root] => Ok(*block_root),
            [] => bail!("no beacon block with payload {block_hash} at slot {slot}"),
            // Two blocks of one slot under one parent are an equivocation of the proposer.
            _ => bail!("unexpected beacon blocks {block_roots:?} with payload {block_hash}"),
        }
    }

    /// Subscribes to the `execution_payload` events. The stream reconnects after a lost
    /// connection, and ends at a response of the beacon node other than an event stream.
    pub(crate) fn execution_payload_events(&self) -> impl Stream<Item = ExecutionPayloadEvent> {
        let url = self.endpoint.join("eth/v1/events?topics=execution_payload");
        // A request timeout covers the whole response, so the stream has a client without one.
        let request = reqwest::Client::new().get(url.expect("the path is valid"));
        let mut events = EventSource::new(request).expect("the request has no body");
        events.set_retry_policy(Box::new(Constant::new(RETRY_DELAY, None)));
        events.filter_map(|event| match event {
            Ok(Event::Message(message)) if message.event == "execution_payload" => {
                serde_json::from_str(&message.data)
                    .inspect_err(|error| warn!(%error, "execution payload event not decodable"))
                    .ok()
            }
            Ok(_) => None,
            Err(error) => {
                warn!(%error, "execution payload stream lost");
                None
            }
        })
    }

    pub(crate) async fn post_execution_proofs(&self, body: Vec<u8>) -> anyhow::Result<()> {
        let url = self.endpoint.join("eth/v1/beacon/execution_proofs")?;
        let request = self.http.post(url).body(body);
        let request = request.header(CONTENT_TYPE, "application/octet-stream");
        error_for_status(request.send().await?).await?;
        Ok(())
    }
}

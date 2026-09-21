//! Client of the beacon API of the CL. It reads the chain spec, the genesis validators root, the
//! public key of a validator, and a block by root. It forwards every other request.

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use axum::{
    body::Bytes,
    http::{HeaderValue, Method, StatusCode, header::CONTENT_TYPE},
};
use lighthouse_bls::PublicKey;
use lighthouse_types::{
    ChainSpec, ForkName, ForkVersionDecode, Hash256, MainnetEthSpec, SignedBeaconBlock,
};
use serde::{Deserialize, de::DeserializeOwned};
use url::Url;

/// Timeout of a beacon API read.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

/// Client of the beacon API of the CL.
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
    genesis_validators_root: Hash256,
}

#[derive(Deserialize)]
struct ValidatorData {
    validator: Validator,
}

#[derive(Deserialize)]
struct Validator {
    #[serde(with = "serde_utils::hex_vec")]
    pubkey: Vec<u8>,
}

impl BeaconNodeClient {
    /// Creates the client of the beacon API at the endpoint.
    pub(crate) fn new(endpoint: Url) -> Self {
        Self {
            endpoint,
            http_client: reqwest::Client::new(),
        }
    }

    /// Returns the signed beacon block of a root, decoded from SSZ by its fork.
    pub(crate) async fn block(
        &self,
        root: Hash256,
    ) -> anyhow::Result<SignedBeaconBlock<MainnetEthSpec>> {
        let url = self
            .endpoint
            .join(&format!("eth/v2/beacon/blocks/{root}"))?;
        let response = self
            .http_client
            .get(url.clone())
            .timeout(REQUEST_TIMEOUT)
            .header("Accept", "application/octet-stream")
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("GET {url}: {status}: {body}");
        }
        let fork_name: ForkName = response
            .headers()
            .get("Eth-Consensus-Version")
            .context("missing Eth-Consensus-Version")?
            .to_str()?
            .parse()
            .map_err(|error: String| anyhow!("{error}"))?;
        let bytes = response.bytes().await?;
        SignedBeaconBlock::from_ssz_bytes_by_fork(&bytes, fork_name)
            .map_err(|error| anyhow!("{error:?}"))
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

    /// Returns the public key of a validator in the head state.
    pub(crate) async fn validator_pubkey(&self, validator_index: u64) -> anyhow::Result<PublicKey> {
        let path = format!("eth/v1/beacon/states/head/validators/{validator_index}");
        let validator: ValidatorData = self
            .get(&path)
            .await?
            .with_context(|| format!("validator {validator_index} not found"))?;
        PublicKey::deserialize(&validator.validator.pubkey)
            .map_err(|error| anyhow!("validator {validator_index} pubkey: {error:?}"))
    }

    /// Returns the genesis validators root of the beacon node.
    pub(crate) async fn genesis_validators_root(&self) -> anyhow::Result<Hash256> {
        let genesis: Genesis = self
            .get("eth/v1/beacon/genesis")
            .await?
            .context("genesis not found")?;
        Ok(genesis.genesis_validators_root)
    }

    /// Forwards a request to the CL and returns the response status, content type, and the
    /// response itself, whose body streams to the caller.
    pub(crate) async fn forward(
        &self,
        method: Method,
        path_and_query: &str,
        body: Bytes,
    ) -> anyhow::Result<(StatusCode, Option<HeaderValue>, reqwest::Response)> {
        let url = self.endpoint.join(path_and_query)?;
        let response = self
            .http_client
            .request(method, url)
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let content_type = response.headers().get(CONTENT_TYPE).cloned();
        Ok((status, content_type, response))
    }

    /// Fetches the `data` of a beacon API resource, or `None` when it does not exist.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<Option<T>> {
        let url = self.endpoint.join(path)?;
        let response = self
            .http_client
            .get(url.clone())
            .timeout(REQUEST_TIMEOUT)
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

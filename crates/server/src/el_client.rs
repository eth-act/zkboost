//! EL JSON-RPC client wrapping the `debug_chainConfig` and
//! `debug_executionWitnessByBlockHash` RPC methods.

use alloy_genesis::ChainConfig as AlloyChainConfig;
use alloy_rpc_types_debug::ExecutionWitness;
use anyhow::Context;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::Url;
use zkboost_types::Hash256;

/// Execution layer JSON-RPC client.
#[derive(Debug)]
pub struct ElClient {
    url: Url,
    http_client: reqwest::Client,
}

impl ElClient {
    /// Create a new EL client. `default_headers` are applied to every request.
    pub fn new(url: Url, default_headers: reqwest::header::HeaderMap) -> anyhow::Result<Self> {
        let http_client = reqwest::Client::builder()
            .default_headers(default_headers)
            .build()
            .context("build EL HTTP client")?;
        Ok(Self { url, http_client })
    }

    /// Return url of the EL client.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Send a JSON-RPC request to the execution layer node.
    ///
    /// Serializes the request, sends it to the endpoint, and deserializes the response.
    /// Returns `None` if the RPC response has a null `result`.
    async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &'static str,
        params: P,
    ) -> Result<Option<(R, usize)>, Error> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method,
            params,
            id: 1,
        };

        let response = self
            .http_client
            .post(self.url.as_str())
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Error::Rpc {
                code: response.status().as_u16() as i64,
                message: response.text().await.unwrap_or_default(),
            });
        }

        let bytes = response.bytes().await?;
        let response_size = bytes.len();
        let rpc_response: JsonRpcResponse<R> = serde_json::from_slice(&bytes)?;

        if let Some(error) = rpc_response.error {
            return Err(Error::Rpc {
                code: error.code,
                message: error.message,
            });
        }

        match rpc_response.result {
            Some(value) => Ok(Some((value, response_size))),
            None => Ok(None),
        }
    }

    /// Fetch chain config.
    pub async fn get_chain_config(&self) -> Result<Option<AlloyChainConfig>, Error> {
        let result = self.request("debug_chainConfig", ()).await?;
        Ok(result.map(|(chain_config, _)| chain_config))
    }

    /// Fetch execution witness for a block, returning the witness and the raw response size.
    pub async fn get_execution_witness_by_hash(
        &self,
        block_hash: Hash256,
    ) -> Result<Option<(ExecutionWitness, usize)>, Error> {
        self.request("debug_executionWitnessByBlockHash", (block_hash,))
            .await
    }
}

/// JSON-RPC request structure.
#[derive(Debug, Clone, Serialize)]
struct JsonRpcRequest<T> {
    jsonrpc: &'static str,
    method: &'static str,
    params: T,
    id: u64,
}

/// JSON-RPC response structure.
#[derive(Debug, Clone, Deserialize)]
struct JsonRpcResponse<T> {
    /// The result payload, if the call succeeded.
    result: Option<T>,
    /// The error payload, if the call failed.
    error: Option<JsonRpcError>,
}

/// JSON-RPC error structure.
#[derive(Debug, Clone, Deserialize)]
struct JsonRpcError {
    /// Error code.
    code: i64,
    /// Error message.
    message: String,
}

/// Errors that can occur when communicating with an EL JSON-RPC endpoint.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// HTTP transport error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// Response deserialization error.
    #[error("deserialize error: {0}")]
    Deserialize(#[from] serde_json::Error),
    /// JSON-RPC level error returned by the node.
    #[error("RPC error {code}: {message}")]
    Rpc {
        /// Error code.
        code: i64,
        /// Error message.
        message: String,
    },
}

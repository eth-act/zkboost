//! EL JSON-RPC client wrapping `debug_chainConfig`, `eth_getBlockByHash`, and
//! `debug_executionWitnessByBlockHash` RPC methods.

use alloy_genesis::ChainConfig;
use reth_ethereum_primitives::{Block, TransactionSigned};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use stateless::ExecutionWitness;
use url::Url;
use zkboost_types::Hash256;

/// Execution layer JSON-RPC client.
#[derive(Debug)]
pub struct ElClient {
    url: Url,
    http_client: reqwest::Client,
}

impl ElClient {
    /// Create a new EL client.
    pub fn new(url: Url) -> Self {
        Self {
            url,
            http_client: reqwest::Client::new(),
        }
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
    ) -> Result<Option<R>, Error> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method,
            params,
            id: 1,
        };

        let response = self
            .http_client
            .post(self.url.clone())
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Error::Rpc {
                code: response.status().as_u16() as i64,
                message: response.text().await.unwrap_or_default(),
            });
        }

        let rpc_response: JsonRpcResponse<R> = response.json().await?;

        if let Some(error) = rpc_response.error {
            return Err(Error::Rpc {
                code: error.code,
                message: error.message,
            });
        }

        match rpc_response.result {
            Some(value) => Ok(Some(value)),
            None => Ok(None),
        }
    }

    /// Fetch chain config.
    pub async fn get_chain_config(&self) -> Result<Option<ChainConfig>, Error> {
        self.request("debug_chainConfig", ()).await
    }

    /// Fetch a block by hash.
    pub async fn get_block_by_hash(&self, block_hash: Hash256) -> Result<Option<Block>, Error> {
        let block: Option<alloy_rpc_types_eth::Block<TransactionSigned>> = self
            .request("eth_getBlockByHash", (block_hash, true))
            .await?;
        Ok(block.map(|block| block.into_consensus()))
    }

    /// Fetch execution witness for a block.
    pub async fn get_execution_witness_by_hash(
        &self,
        block_hash: Hash256,
    ) -> Result<Option<ExecutionWitness>, Error> {
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
    /// JSON-RPC level error returned by the node.
    #[error("RPC error {code}: {message}")]
    Rpc {
        /// Error code.
        code: i64,
        /// Error message.
        message: String,
    },
}

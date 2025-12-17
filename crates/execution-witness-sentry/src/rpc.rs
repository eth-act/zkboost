//! Execution layer JSON-RPC client.

use alloy_rpc_types_eth::Block;
use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};

/// Client for interacting with an execution layer node via JSON-RPC.
#[derive(Debug, Clone)]
pub struct ElClient {
    url: Url,
    client: Client,
}

impl ElClient {
    /// Create a new client for the given RPC URL.
    pub fn new(url: Url) -> Self {
        Self {
            url,
            client: Client::new(),
        }
    }

    /// Fetch a block by its hash.
    ///
    /// Returns `None` if the block is not found.
    pub async fn get_block_by_hash(&self, block_hash: &str) -> Result<Option<Block>> {
        self.call("eth_getBlockByHash", (block_hash, false)).await
    }

    /// Fetch the execution witness for a block by number.
    ///
    /// Returns `None` if the witness is not available.
    pub async fn get_execution_witness(
        &self,
        block_number: u64,
    ) -> Result<Option<serde_json::Value>> {
        let block_hex = format!("0x{block_number:x}");
        self.call("debug_executionWitness", (block_hex,)).await
    }

    /// Make a JSON-RPC call.
    async fn call<P, R>(&self, method: &'static str, params: P) -> Result<Option<R>>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method,
            params,
            id: 1,
        };

        let response: JsonRpcResponse<R> = self
            .client
            .post(self.url.clone())
            .json(&request)
            .send()
            .await?
            .json()
            .await?;

        if let Some(error) = response.error {
            return Err(Error::Rpc {
                code: error.code,
                message: error.message,
            });
        }

        Ok(response.result)
    }
}

#[derive(Debug, Serialize)]
struct JsonRpcRequest<P> {
    jsonrpc: &'static str,
    method: &'static str,
    params: P,
    id: u64,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse<R> {
    result: Option<R>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

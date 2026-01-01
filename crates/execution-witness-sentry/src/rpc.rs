//! JSON-RPC client for execution layer nodes.

use alloy_rpc_types_eth::Block;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};
use crate::storage::compress_gzip;

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
pub struct JsonRpcResponse<T> {
    pub result: Option<T>,
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC error structure.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

/// Execution layer JSON-RPC client.
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

    /// Fetch a block by hash. Returns the block and its gzipped JSON.
    pub async fn get_block_by_hash(&self, block_hash: &str) -> Result<Option<(Block, Vec<u8>)>> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "eth_getBlockByHash",
            params: (block_hash, false),
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

        let rpc_response: JsonRpcResponse<Block> = response.json().await?;

        if let Some(error) = rpc_response.error {
            return Err(Error::Rpc {
                code: error.code,
                message: error.message,
            });
        }

        match rpc_response.result {
            Some(block) => {
                let json_bytes = serde_json::to_vec(&block)?;
                let gzipped = compress_gzip(&json_bytes)?;
                Ok(Some((block, gzipped)))
            }
            None => Ok(None),
        }
    }

    /// Fetch execution witness for a block. Returns the witness and its gzipped JSON.
    pub async fn get_execution_witness(
        &self,
        block_number: u64,
    ) -> Result<Option<(serde_json::Value, Vec<u8>)>> {
        let block_num_hex = format!("0x{:x}", block_number);
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "debug_executionWitness",
            params: (block_num_hex,),
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

        let rpc_response: JsonRpcResponse<serde_json::Value> = response.json().await?;

        if let Some(error) = rpc_response.error {
            return Err(Error::Rpc {
                code: error.code,
                message: error.message,
            });
        }

        match rpc_response.result {
            Some(witness) => {
                let json_bytes = serde_json::to_vec(&witness)?;
                let gzipped = compress_gzip(&json_bytes)?;
                Ok(Some((witness, gzipped)))
            }
            None => Ok(None),
        }
    }
}

/// Execution proof to submit to CL nodes.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionProof {
    pub proof_id: u32,
    pub slot: String,
    pub block_hash: String,
    pub block_root: String,
    pub proof_data: Vec<u8>,
}

/// Consensus layer HTTP API client.
pub struct ClClient {
    url: Url,
    http_client: reqwest::Client,
}

/// Syncing status response.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncingResponse {
    pub data: SyncingData,
}

/// Syncing status data.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncingData {
    pub head_slot: String,
    pub is_syncing: bool,
    pub is_optimistic: Option<bool>,
}

/// Block header response.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockHeaderResponse {
    pub data: BlockHeaderData,
}

/// Block header data.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockHeaderData {
    pub root: String,
}

impl ClClient {
    /// Create a new CL client.
    pub fn new(url: Url) -> Self {
        Self {
            url,
            http_client: reqwest::Client::new(),
        }
    }

    /// Get node syncing status.
    pub async fn get_syncing(&self) -> Result<SyncingResponse> {
        let url = self.url.join("eth/v1/node/syncing")?;
        let response = self.http_client.get(url).send().await?;
        Ok(response.json().await?)
    }

    /// Get block header for a slot.
    pub async fn get_block_header(&self, slot: u64) -> Result<Option<BlockHeaderResponse>> {
        let url = self.url.join(&format!("eth/v1/beacon/headers/{}", slot))?;
        let response = self.http_client.get(url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        Ok(Some(response.json().await?))
    }

    /// Submit an execution proof.
    pub async fn submit_execution_proof(&self, proof: &ExecutionProof) -> Result<()> {
        let url = self.url.join("eth/v1/beacon/pool/execution_proofs")?;

        let response = self.http_client.post(url).json(proof).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Rpc {
                code: status.as_u16() as i64,
                message: body,
            });
        }

        Ok(())
    }
}

/// Generate random proof bytes.
pub fn generate_random_proof(proof_id: u32) -> Vec<u8> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    let mut proof = vec![0u8; 32];
    for (i, byte) in proof.iter_mut().enumerate() {
        *byte = ((seed >> (i % 8)) ^ (i as u64)) as u8;
    }
    proof[31] = proof_id as u8;
    proof
}

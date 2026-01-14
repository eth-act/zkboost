//! JSON-RPC client for execution layer nodes.

use alloy_rpc_types_eth::Block;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    error::{Error, Result},
    storage::compress_gzip,
};

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
    name: String,
    url: Url,
    http_client: reqwest::Client,
}

impl ElClient {
    /// Create a new EL client.
    pub fn new(name: String, url: Url) -> Self {
        Self {
            name,
            url,
            http_client: reqwest::Client::new(),
        }
    }

    /// Return name of the EL client.
    pub fn name(&self) -> &str {
        &self.name
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
    pub async fn get_execution_witness_by_hash(
        &self,
        block_hash: &str,
    ) -> Result<Option<(serde_json::Value, Vec<u8>)>> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method: "debug_executionWitnessByBlockHash",
            params: (block_hash,),
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
    pub proof_id: u8,
    pub slot: u64,
    pub block_hash: String,
    pub block_root: String,
    pub proof_data: Vec<u8>,
}

/// Consensus layer HTTP API client.
#[derive(Clone)]
pub struct ClClient {
    name: String,
    url: Url,
    http_client: reqwest::Client,
}

/// Block response with execution payload.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockResponse {
    pub data: BlockData,
}

/// Block data.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockData {
    pub message: BlockMessage,
}

/// Block message.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockMessage {
    pub body: BlockBody,
}

/// Block body.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockBody {
    pub execution_payload: Option<ExecutionPayload>,
}

/// Execution payload (minimal fields).
#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionPayload {
    pub block_hash: String,
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

/// Node identity response.
#[derive(Debug, Clone, Deserialize)]
pub struct IdentityResponse {
    pub data: IdentityData,
}

/// Node identity data.
#[derive(Debug, Clone, Deserialize)]
pub struct IdentityData {
    pub enr: String,
}

impl ClClient {
    /// Create a new CL client.
    pub fn new(name: String, url: Url) -> Self {
        Self {
            url,
            name,
            http_client: reqwest::Client::new(),
        }
    }

    /// Return name of the CL client.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return url of the CL client.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Get node syncing status.
    pub async fn get_syncing(&self) -> Result<SyncingResponse> {
        let url = self.url.join("eth/v1/node/syncing")?;
        let response = self.http_client.get(url).send().await?;
        Ok(response.json().await?)
    }

    /// Get block header for a slot.
    pub async fn get_block_header(&self, slot: u64) -> Result<Option<BlockHeaderResponse>> {
        let url = self.url.join(&format!("eth/v1/beacon/headers/{slot}"))?;
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

    /// Get node identity (including ENR).
    pub async fn get_identity(&self) -> Result<IdentityResponse> {
        let url = self.url.join("eth/v1/node/identity")?;
        let response = self.http_client.get(url).send().await?;
        Ok(response.json().await?)
    }

    /// Check if the node has zkvm enabled by inspecting its ENR.
    pub async fn is_zkvm_enabled(&self) -> Result<bool> {
        let identity = self.get_identity().await?;
        Ok(enr_has_zkvm(&identity.data.enr))
    }

    /// Get the execution block hash for a beacon block.
    pub async fn get_block_execution_hash(&self, block_root: &str) -> Result<Option<String>> {
        let url = self
            .url
            .join(&format!("eth/v2/beacon/blocks/{block_root}"))?;
        let response = self.http_client.get(url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let block_response: BlockResponse = response.json().await?;
        Ok(block_response
            .data
            .message
            .body
            .execution_payload
            .map(|p| p.block_hash))
    }

    /// Get the current head slot.
    pub async fn get_head_slot(&self) -> Result<u64> {
        let syncing = self.get_syncing().await?;
        syncing
            .data
            .head_slot
            .parse()
            .map_err(|e| Error::Config(format!("Invalid head slot: {e}")))
    }

    /// Get block info (slot, block_root, execution_block_hash) for a given slot.
    /// Returns None if the slot is empty (no block).
    pub async fn get_block_info(&self, slot: u64) -> Result<Option<BlockInfo>> {
        let url = self.url.join(&format!("eth/v2/beacon/blocks/{slot}"))?;
        let response = self.http_client.get(url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            return Err(Error::Rpc {
                code: response.status().as_u16() as i64,
                message: response.text().await.unwrap_or_default(),
            });
        }

        let block_response: BlockResponse = response.json().await?;
        let execution_block_hash = block_response
            .data
            .message
            .body
            .execution_payload
            .map(|p| p.block_hash);

        // Get the block root from headers endpoint
        let header_url = self.url.join(&format!("eth/v1/beacon/headers/{slot}"))?;
        let header_response = self.http_client.get(header_url).send().await?;

        if header_response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let header: BlockHeaderResponse = header_response.json().await?;

        Ok(Some(BlockInfo {
            slot,
            block_root: header.data.root,
            execution_block_hash,
        }))
    }
}

/// Block info for backfill.
#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub slot: u64,
    pub block_root: String,
    pub execution_block_hash: Option<String>,
}

/// The ENR field specifying whether zkVM execution proofs are enabled.
const ZKVM_ENABLED_ENR_KEY: &str = "zkvm";

/// Check if an ENR string contains the zkvm flag.
fn enr_has_zkvm(enr_str: &str) -> bool {
    use std::str::FromStr;

    use discv5::enr::{CombinedKey, Enr};

    match Enr::<CombinedKey>::from_str(enr_str) {
        Ok(enr) => enr
            .get_decodable::<bool>(ZKVM_ENABLED_ENR_KEY)
            .and_then(|result| result.ok())
            .unwrap_or(false),
        Err(_) => false,
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

//! JSON-RPC client for execution layer nodes.

use alloy_genesis::ChainConfig;
use reth_ethereum_primitives::{Block, TransactionSigned};
use reth_stateless::ExecutionWitness;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::Url;
use zkboost_client::zkboostClient;
use zkboost_ethereum_el_input::ElInput;
use zkboost_ethereum_el_types::ElProofType;
use zkboost_types::ProofGenId;

use crate::error::{Error, Result};

pub type Hash256 = alloy_primitives::B256;

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
    ) -> Result<Option<R>> {
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
            Some(config) => Ok(Some(config)),
            None => Ok(None),
        }
    }

    /// Fetch chain config.
    pub async fn get_chain_config(&self) -> Result<Option<ChainConfig>> {
        self.request("debug_chainConfig", ()).await
    }

    /// Fetch a block by hash. Returns the block and its gzipped JSON.
    pub async fn get_block_by_hash(&self, block_hash: Hash256) -> Result<Option<Block>> {
        let block: Option<alloy_rpc_types_eth::Block<TransactionSigned>> = self
            .request("eth_getBlockByHash", (block_hash, false))
            .await?;
        Ok(block.map(|block| block.into_consensus()))
    }

    /// Fetch execution witness for a block. Returns the witness and its gzipped JSON.
    pub async fn get_execution_witness_by_hash(
        &self,
        block_hash: Hash256,
    ) -> Result<Option<ExecutionWitness>> {
        self.request("debug_executionWitnessByBlockHash", (block_hash,))
            .await
    }
}

/// Execution proof to submit to CL nodes.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionProof {
    pub proof_id: u8,
    pub slot: u64,
    pub block_hash: Hash256,
    pub block_root: Hash256,
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
    pub block_hash: Hash256,
}

/// Syncing status response.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncingResponse {
    pub data: SyncingData,
}

/// Syncing status data.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncingData {
    #[serde(with = "serde_utils::quoted_u64")]
    pub head_slot: u64,
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
    pub root: Hash256,
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
    pub async fn get_block_execution_hash(&self, block_root: Hash256) -> Result<Option<Hash256>> {
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
        Ok(syncing.data.head_slot)
    }

    /// Get block info (slot, block_root, execution_block_hash) for a given slot.
    /// Returns None if the slot is empty (no block).
    pub async fn get_block_info(&self, slot: u64) -> Result<Option<BlockInfo>> {
        let Some(header) = self.get_block_header(slot).await? else {
            return Ok(None);
        };

        let block_root = header.data.root;
        let execution_block_hash = self.get_block_execution_hash(block_root).await?;

        Ok(Some(BlockInfo {
            slot,
            block_root,
            execution_block_hash,
        }))
    }
}

/// Block info for backfill.
#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub slot: u64,
    pub block_root: Hash256,
    pub execution_block_hash: Option<Hash256>,
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

/// Client for communicating with a proof engine.
pub struct ProofEngineClient {
    /// URL of the proof engine.
    pub url: Url,
    /// Proof types supported by this engine.
    pub proof_types: Vec<ElProofType>,
    /// Underlying client for API calls.
    client: zkboostClient,
}

impl ProofEngineClient {
    /// Create a new proof engine client.
    pub fn new(url: Url, proof_types: Vec<ElProofType>) -> anyhow::Result<Self> {
        let client = zkboostClient::new(url.clone())?;
        Ok(Self {
            url,
            proof_types,
            client,
        })
    }

    /// Return the proof types supported by this engine.
    pub fn proof_types(&self) -> &[ElProofType] {
        &self.proof_types
    }

    /// Submit a proof generation request to the zkboost server.
    ///
    /// Converts the execution input to zkVM format and submits it to the
    /// configured proof engine. Returns a proof generation ID that can be
    /// used to identity webhook callbacks.
    pub async fn request_proof(
        &self,
        proof_type: &ElProofType,
        el_input: &ElInput,
    ) -> anyhow::Result<ProofGenId> {
        Ok(self
            .client
            .prove(
                proof_type.to_string(),
                el_input.to_zkvm_input(proof_type.el(), true)?.stdin,
            )
            .await?
            .proof_gen_id)
    }
}

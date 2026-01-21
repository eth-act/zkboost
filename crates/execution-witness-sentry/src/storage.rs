//! Block data storage utilities.

use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::PathBuf,
};

use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use reth_ethereum_primitives::Block;
use reth_stateless::ExecutionWitness;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use zkboost_ethereum_el_types::ElProofType;

use crate::error::Result;

/// Metadata stored alongside block data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockMetadata {
    /// EL block hash
    pub block_hash: String,
    /// EL block number
    pub block_number: Option<u64>,
    /// Gas used in the block
    pub gas_used: Option<u64>,
    /// CL slot number (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    /// CL beacon block root (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beacon_block_root: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ElBlockWitness {
    /// EL block
    pub block: Block,
    /// EL block execution witness
    pub witness: ExecutionWitness,
}

/// A saved proof that can be loaded for backfill.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde_as]
pub struct SavedProof {
    /// Proof type
    pub proof_type: ElProofType,
    #[serde_as(as = "Base64")]
    /// Proof data
    pub proof_data: Vec<u8>,
}

/// Compress data using gzip.
pub fn compress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Decompress gzip data.
pub fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

/// Manages block data storage on disk.
pub struct BlockStorage {
    output_dir: PathBuf,
    chain: String,
    retained: Option<VecDeque<String>>,
}

impl BlockStorage {
    /// Create a new block storage manager.
    pub fn new(
        output_dir: impl Into<PathBuf>,
        chain: impl Into<String>,
        retain: Option<u64>,
    ) -> Self {
        Self {
            output_dir: output_dir.into(),
            chain: chain.into(),
            retained: retain.map(|retain| VecDeque::with_capacity(retain as usize)),
        }
    }

    /// Get the directory path for a block hash.
    pub fn block_dir(&self, block_hash: &str) -> PathBuf {
        self.output_dir.join(&self.chain).join(block_hash)
    }

    /// Save block data to disk (without CL info - will be updated later).
    pub fn save_block(&mut self, el_data: &ElBlockWitness) -> Result<()> {
        let block_number = el_data.block.header.number;
        let block_hash = el_data.block.hash_slow().to_string();
        let gas_used = el_data.block.header.gas_used;

        let block_dir = self.block_dir(&block_hash);
        std::fs::create_dir_all(&block_dir)?;

        // Load existing metadata or create new
        let metadata_path = block_dir.join("metadata.json");
        let mut metadata = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)?;
            serde_json::from_str(&content)?
        } else {
            BlockMetadata {
                block_hash: block_hash.to_string(),
                ..Default::default()
            }
        };

        // Update with EL info
        metadata.block_number = Some(block_number);
        metadata.gas_used = Some(gas_used);

        // Save updated metadata
        std::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;

        // Write combined block + witness data
        let data_path = block_dir.join("data.json.gz");
        std::fs::write(data_path, compress_gzip(&serde_json::to_vec(&el_data)?)?)?;

        if let Some(expired) = self.retained.as_mut().and_then(|retained| {
            let expired = (retained.len() == retained.capacity()).then(|| retained.pop_front());
            retained.push_back(block_hash);
            expired.flatten()
        }) {
            self.delete_old_block(&expired)?;
        }

        Ok(())
    }

    pub fn save_proof(
        &self,
        slot: u64,
        beacon_block_root: &str,
        block_hash: &str,
        proof_type: ElProofType,
        proof_data: &[u8],
    ) -> Result<()> {
        let block_dir = self.block_dir(block_hash);
        std::fs::create_dir_all(&block_dir)?;

        let metadata_path = block_dir.join("metadata.json");
        let mut metadata = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)?;
            serde_json::from_str(&content)?
        } else {
            BlockMetadata {
                block_hash: block_hash.to_string(),
                ..Default::default()
            }
        };

        metadata.slot = Some(slot);
        metadata.beacon_block_root = Some(beacon_block_root.to_string());

        let proof_dir = block_dir.join("proof");
        std::fs::create_dir_all(&proof_dir)?;

        let proof_path = proof_dir.join(format!("{proof_type}.json"));
        std::fs::write(
            &proof_path,
            serde_json::to_string(&SavedProof {
                proof_type,
                proof_data: proof_data.to_vec(),
            })?,
        )?;

        std::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;

        Ok(())
    }

    pub fn load_proof(
        &self,
        block_hash: &str,
        proof_type: ElProofType,
    ) -> Result<Option<SavedProof>> {
        let block_dir = self.block_dir(block_hash);

        if !block_dir.exists() {
            return Ok(None);
        }

        let proof_dir = block_dir.join("proof");
        if !proof_dir.exists() {
            return Ok(None);
        }

        let proof_path = proof_dir.join(format!("{proof_type}.json"));
        if !proof_path.exists() {
            return Ok(None);
        }

        let bytes = std::fs::read(&proof_path)?;
        let proof: SavedProof = serde_json::from_slice(&bytes)?;
        Ok(Some(proof))
    }

    /// Load metadata for a given block hash.
    pub fn load_metadata(&self, block_hash: &str) -> Result<Option<BlockMetadata>> {
        let block_dir = self.block_dir(block_hash);
        let metadata_path = block_dir.join("metadata.json");

        if !metadata_path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&metadata_path)?;
        Ok(Some(serde_json::from_str(&content)?))
    }

    /// Delete an old block directory.
    fn delete_old_block(&self, block_hash: &str) -> Result<()> {
        let old_dir = self.block_dir(block_hash);
        if old_dir.exists() {
            std::fs::remove_dir_all(old_dir)?;
        }
        Ok(())
    }

    /// Load block and witness data from disk.
    pub fn load_block_and_witness(
        &self,
        block_hash: &str,
    ) -> Result<Option<(Block, alloy_rpc_types_debug::ExecutionWitness)>> {
        let block_dir = self.block_dir(block_hash);
        let data_path = block_dir.join("data.json.gz");

        if !data_path.exists() {
            return Ok(None);
        }

        Ok(serde_json::from_slice(&decompress_gzip(&std::fs::read(data_path)?)?).map(Some)?)
    }

    /// Get the chain directory path.
    pub fn chain_dir(&self) -> PathBuf {
        self.output_dir.join(&self.chain)
    }
}

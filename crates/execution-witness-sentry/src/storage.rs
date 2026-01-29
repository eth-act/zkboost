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
use zkboost_ethereum_el_types::ElProofType;

use crate::{error::Result, rpc::Hash256};

/// Metadata stored alongside block data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockMetadata {
    /// EL block hash
    pub block_hash: Hash256,
    /// EL block number
    pub block_number: Option<u64>,
    /// Gas used in the block
    pub gas_used: Option<u64>,
    /// CL slot number (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    /// CL beacon block root (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beacon_block_root: Option<Hash256>,
}

/// EL block and execution witness data.
#[derive(Serialize, Deserialize)]
pub struct ElBlockWitness {
    /// EL block.
    pub block: Block,
    /// Execution witness.
    pub witness: ExecutionWitness,
}

/// A saved proof that can be loaded for backfill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proof {
    /// Proof type
    pub proof_type: ElProofType,
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

/// Manages EL block data and proof storage on disk.
///
/// `BlockStorage` organizes data in a directory structure:
/// ```text
/// {output_dir}/{chain}/{block_hash}/
/// ├── metadata.json     # Block metadata (slot, block number, etc.)
/// ├── data.json.gz      # Compressed block + witness data
/// └── proof/
///     └── {proof_type}.json  # Compressed proofs by type
/// ```
///
/// ## Retention Policy
///
/// When configured with a retention limit, old block directories are automatically
/// deleted as new blocks are saved, maintaining a sliding window of the most
/// recent blocks. This prevents unbounded disk usage.
pub struct BlockStorage {
    /// Root directory for all stored data.
    output_dir: PathBuf,
    /// Chain identifier used as a subdirectory name (e.g., "mainnet", "holesky").
    chain: String,
    /// Queue of block hashes for retention tracking. When full, the oldest
    /// entry is removed and its directory deleted. `None` disables retention.
    retained: Option<VecDeque<Hash256>>,
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

    /// Get the chain directory path.
    pub fn chain_dir(&self) -> PathBuf {
        self.output_dir.join(&self.chain)
    }

    /// Get the directory path for a block hash.
    pub fn block_dir(&self, block_hash: Hash256) -> PathBuf {
        self.chain_dir().join(block_hash.to_string())
    }

    /// Save CL block header data to disk.
    pub fn save_cl_data(
        &mut self,
        block_hash: Hash256,
        slot: u64,
        beacon_block_root: Hash256,
    ) -> Result<()> {
        let block_dir = self.block_dir(block_hash);
        std::fs::create_dir_all(&block_dir)?;

        // Load existing metadata or create new
        let metadata_path = block_dir.join("metadata.json");
        let mut metadata = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)?;
            serde_json::from_str(&content)?
        } else {
            BlockMetadata {
                block_hash,
                ..Default::default()
            }
        };

        metadata.slot = Some(slot);
        metadata.beacon_block_root = Some(beacon_block_root);

        std::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;

        Ok(())
    }

    /// Save EL block and witness to disk.
    pub fn save_el_data(&mut self, el_data: &ElBlockWitness) -> Result<()> {
        let block_number = el_data.block.header.number;
        let block_hash = el_data.block.hash_slow();
        let gas_used = el_data.block.header.gas_used;

        let block_dir = self.block_dir(block_hash);
        std::fs::create_dir_all(&block_dir)?;

        // Load existing metadata or create new
        let metadata_path = block_dir.join("metadata.json");
        let mut metadata = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)?;
            serde_json::from_str(&content)?
        } else {
            BlockMetadata {
                block_hash,
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
        let compressed = compress_gzip(&serde_json::to_vec(&el_data)?)?;
        std::fs::write(data_path, compressed)?;

        if let Some(expired) = self.retained.as_mut().and_then(|retained| {
            let expired = (retained.len() == retained.capacity()).then(|| retained.pop_front());
            retained.push_back(block_hash);
            expired.flatten()
        }) {
            self.delete_old_block(expired)?;
        }

        Ok(())
    }

    /// Save a execution proof to disk.
    ///
    /// Proofs are stored in a `proof/` subdirectory under the block's directory,
    /// with the filename based on the proof type.
    pub fn save_proof(
        &mut self,
        block_hash: Hash256,
        proof_type: ElProofType,
        proof_data: &[u8],
    ) -> Result<()> {
        let proof = Proof {
            proof_type,
            proof_data: proof_data.to_vec(),
        };

        let block_dir = self.block_dir(block_hash);
        std::fs::create_dir_all(&block_dir)?;

        let proof_dir = block_dir.join("proof");
        std::fs::create_dir_all(&proof_dir)?;

        let proof_path = proof_dir.join(format!("{proof_type}.json"));
        let compressed = compress_gzip(&serde_json::to_vec(&proof)?)?;
        std::fs::write(&proof_path, compressed)?;

        Ok(())
    }

    /// Load metadata for a given block hash.
    pub fn load_metadata(&self, block_hash: Hash256) -> Result<Option<BlockMetadata>> {
        let block_dir = self.block_dir(block_hash);
        let metadata_path = block_dir.join("metadata.json");

        if !metadata_path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&metadata_path)?;
        Ok(Some(serde_json::from_str(&content)?))
    }

    /// Load EL block and witness data from disk.
    pub fn load_el_data(&self, block_hash: Hash256) -> Result<Option<ElBlockWitness>> {
        let block_dir = self.block_dir(block_hash);
        let data_path = block_dir.join("data.json.gz");

        if !data_path.exists() {
            return Ok(None);
        }

        let compressed = std::fs::read(data_path)?;
        let el_data = serde_json::from_slice(&decompress_gzip(&compressed)?)?;

        Ok(Some(el_data))
    }

    /// Load proof from disk.
    ///
    /// Returns `None` if no proof of the specified type exists for the given block.
    pub fn load_proof(
        &self,
        block_hash: Hash256,
        proof_type: ElProofType,
    ) -> Result<Option<Proof>> {
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

        let compressed = std::fs::read(proof_path)?;
        let proof = serde_json::from_slice(&decompress_gzip(&compressed)?)?;

        Ok(Some(proof))
    }

    /// Delete an old block directory.
    fn delete_old_block(&self, block_hash: Hash256) -> Result<()> {
        let old_dir = self.block_dir(block_hash);
        if old_dir.exists() {
            std::fs::remove_dir_all(old_dir)?;
        }
        Ok(())
    }
}

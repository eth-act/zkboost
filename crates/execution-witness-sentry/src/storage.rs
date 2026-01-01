//! Block data storage utilities.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use alloy_rpc_types_eth::Block;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Metadata stored alongside block data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockMetadata {
    /// EL block hash
    pub block_hash: String,
    /// EL block number
    pub block_number: u64,
    /// Gas used in the block
    pub gas_used: u64,
    /// CL slot number (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    /// CL beacon block root (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_root: Option<String>,
    /// Number of proofs stored
    #[serde(default)]
    pub num_proofs: usize,
}

/// A saved proof that can be loaded for backfill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedProof {
    pub proof_id: u8,
    pub slot: u64,
    pub block_hash: String,
    pub block_root: String,
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

/// Load block data from a gzipped JSON file.
pub fn load_block_data<T: serde::de::DeserializeOwned>(path: impl AsRef<Path>) -> Result<T> {
    let compressed = std::fs::read(path)?;
    let decompressed = decompress_gzip(&compressed)?;
    Ok(serde_json::from_slice(&decompressed)?)
}

/// Manages block data storage on disk.
pub struct BlockStorage {
    output_dir: PathBuf,
    chain: String,
    retain: Option<u64>,
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
            retain,
        }
    }

    /// Get the directory path for a block number.
    pub fn block_dir(&self, block_number: u64) -> PathBuf {
        self.output_dir
            .join(&self.chain)
            .join(block_number.to_string())
    }

    /// Save block data to disk (without CL info - will be updated later).
    pub fn save_block(&self, block: &Block, combined_data: &[u8]) -> Result<()> {
        let block_number = block.header.number;
        let block_hash = format!("{:?}", block.header.hash);
        let gas_used = block.header.gas_used;

        let block_dir = self.block_dir(block_number);
        std::fs::create_dir_all(&block_dir)?;

        // Write metadata (without CL info initially)
        let metadata = BlockMetadata {
            block_hash,
            block_number,
            gas_used,
            slot: None,
            block_root: None,
            num_proofs: 0,
        };
        let metadata_path = block_dir.join("metadata.json");
        std::fs::write(metadata_path, serde_json::to_string_pretty(&metadata)?)?;

        // Write combined block + witness data
        let data_path = block_dir.join("data.json.gz");
        std::fs::write(data_path, combined_data)?;

        // Clean up old blocks if retention is configured
        if let Some(retain) = self.retain
            && block_number > retain
        {
            self.delete_old_block(block_number - retain)?;
        }

        Ok(())
    }

    /// Save proofs and update metadata with CL info.
    /// This is called when we receive CL head event with slot/block_root.
    pub fn save_proofs(
        &self,
        block_number: u64,
        slot: u64,
        block_root: &str,
        block_hash: &str,
        proofs: &[SavedProof],
    ) -> Result<()> {
        let block_dir = self.block_dir(block_number);

        // Create dir if it doesn't exist (in case block wasn't saved yet)
        std::fs::create_dir_all(&block_dir)?;

        // Load existing metadata or create new
        let metadata_path = block_dir.join("metadata.json");
        let mut metadata = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)?;
            serde_json::from_str(&content)?
        } else {
            BlockMetadata {
                block_hash: block_hash.to_string(),
                block_number,
                gas_used: 0,
                slot: None,
                block_root: None,
                num_proofs: 0,
            }
        };

        // Update with CL info
        metadata.slot = Some(slot);
        metadata.block_root = Some(block_root.to_string());
        metadata.num_proofs = proofs.len();

        // Save updated metadata
        std::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;

        // Save proofs
        let proofs_path = block_dir.join("proofs.json");
        std::fs::write(&proofs_path, serde_json::to_string_pretty(&proofs)?)?;

        Ok(())
    }

    /// Load proofs for a given slot.
    /// Searches for a block directory that has matching slot in metadata.
    pub fn load_proofs_by_slot(
        &self,
        slot: u64,
    ) -> Result<Option<(BlockMetadata, Vec<SavedProof>)>> {
        let chain_dir = self.output_dir.join(&self.chain);
        if !chain_dir.exists() {
            return Ok(None);
        }

        // Iterate through block directories to find one with matching slot
        for entry in std::fs::read_dir(&chain_dir)? {
            let entry = entry?;
            let block_dir = entry.path();

            if !block_dir.is_dir() {
                continue;
            }

            let metadata_path = block_dir.join("metadata.json");
            if !metadata_path.exists() {
                continue;
            }

            let content = std::fs::read_to_string(&metadata_path)?;
            let metadata: BlockMetadata = match serde_json::from_str(&content) {
                Ok(m) => m,
                Err(_) => continue,
            };

            if metadata.slot == Some(slot) {
                // Found matching slot, load proofs
                let proofs_path = block_dir.join("proofs.json");
                if proofs_path.exists() {
                    let proofs_content = std::fs::read_to_string(&proofs_path)?;
                    let proofs: Vec<SavedProof> = serde_json::from_str(&proofs_content)?;
                    return Ok(Some((metadata, proofs)));
                } else {
                    return Ok(Some((metadata, vec![])));
                }
            }
        }

        Ok(None)
    }

    /// Load metadata for a given block number.
    pub fn load_metadata(&self, block_number: u64) -> Result<Option<BlockMetadata>> {
        let block_dir = self.block_dir(block_number);
        let metadata_path = block_dir.join("metadata.json");

        if !metadata_path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&metadata_path)?;
        Ok(Some(serde_json::from_str(&content)?))
    }

    /// Delete an old block directory.
    fn delete_old_block(&self, block_number: u64) -> Result<()> {
        let old_dir = self.block_dir(block_number);
        if old_dir.exists() {
            std::fs::remove_dir_all(old_dir)?;
        }
        Ok(())
    }

    /// Get the chain directory path.
    pub fn chain_dir(&self) -> PathBuf {
        self.output_dir.join(&self.chain)
    }
}

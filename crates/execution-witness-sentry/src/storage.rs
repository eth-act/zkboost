//! Block data storage utilities.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use alloy_rpc_types_eth::Block;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::error::Result;

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
    pub fn new(output_dir: impl Into<PathBuf>, chain: impl Into<String>, retain: Option<u64>) -> Self {
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

    /// Save block data to disk.
    pub fn save_block(&self, block: &Block, combined_data: &[u8]) -> Result<()> {
        let block_number = block.header.number;
        let block_hash = format!("{:?}", block.header.hash);
        let gas_used = block.header.gas_used;

        let block_dir = self.block_dir(block_number);
        std::fs::create_dir_all(&block_dir)?;

        // Write metadata
        let metadata = serde_json::json!({
            "block_hash": block_hash,
            "block_number": block_number,
            "gas_used": gas_used,
        });
        let metadata_path = block_dir.join("metadata.json");
        std::fs::write(metadata_path, serde_json::to_string_pretty(&metadata)?)?;

        // Write combined block + witness data
        let data_path = block_dir.join("data.json.gz");
        std::fs::write(data_path, combined_data)?;

        // Clean up old blocks if retention is configured
        if let Some(retain) = self.retain {
            if block_number > retain {
                self.delete_old_block(block_number - retain)?;
            }
        }

        Ok(())
    }

    /// Delete an old block directory.
    fn delete_old_block(&self, block_number: u64) -> Result<()> {
        let old_dir = self.block_dir(block_number);
        if old_dir.exists() {
            std::fs::remove_dir_all(old_dir)?;
        }
        Ok(())
    }
}

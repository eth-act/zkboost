//! Block and witness data persistence.

use std::io::Write;
use std::path::{Path, PathBuf};

use alloy_rpc_types_eth::Block;
use flate2::write::GzEncoder;
use flate2::Compression;
use tracing::{debug, error, info};

use crate::error::Result;

/// Manages storage of block and witness data.
#[derive(Debug, Clone)]
pub struct BlockStorage {
    base_dir: PathBuf,
    chain: String,
    retain: Option<u64>,
}

impl BlockStorage {
    /// Create a new block storage manager.
    ///
    /// - `base_dir`: Base directory for storing data.
    /// - `chain`: Chain identifier (used as subdirectory).
    /// - `retain`: Number of recent blocks to keep (older blocks are deleted).
    pub fn new(base_dir: impl Into<PathBuf>, chain: impl Into<String>, retain: Option<u64>) -> Self {
        Self {
            base_dir: base_dir.into(),
            chain: chain.into(),
            retain,
        }
    }

    /// Save block and witness data for a given block number.
    pub fn save(
        &self,
        block_number: u64,
        block_hash: &str,
        block: &Block,
        witness: &serde_json::Value,
    ) -> Result<()> {
        let block_dir = self.block_dir(block_number);
        std::fs::create_dir_all(&block_dir)?;

        // Write metadata
        let metadata = serde_json::json!({
            "block_hash": block_hash,
            "block_number": block_number,
            "gas_used": block.header.gas_used,
        });
        let metadata_path = block_dir.join("metadata.json");
        std::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;

        // Write combined block + witness as gzipped JSON
        let combined = serde_json::json!({
            "block": block,
            "witness": witness,
        });
        let gzipped = compress_gzip(&serde_json::to_vec(&combined)?)?;
        let data_path = block_dir.join("data.json.gz");
        std::fs::write(&data_path, &gzipped)?;

        info!(
            number = block_number,
            size_bytes = gzipped.len(),
            "Saved block data"
        );

        // Clean up old blocks if retention is configured
        self.cleanup_old_blocks(block_number);

        Ok(())
    }

    /// Get the directory path for a specific block.
    fn block_dir(&self, block_number: u64) -> PathBuf {
        self.base_dir.join(&self.chain).join(block_number.to_string())
    }

    /// Delete blocks older than the retention limit.
    fn cleanup_old_blocks(&self, current_block: u64) {
        let Some(retain) = self.retain else {
            return;
        };

        if current_block <= retain {
            return;
        }

        let old_block_num = current_block - retain;
        let old_block_dir = self.block_dir(old_block_num);

        if old_block_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&old_block_dir) {
                error!(error = %e, number = old_block_num, "Failed to delete old block");
            } else {
                debug!(number = old_block_num, "Deleted old block");
            }
        }
    }
}

/// Compress data using gzip.
pub fn compress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Decompress gzip data.
pub fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let mut decoder = GzDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

/// Load block and witness data from a saved file.
pub fn load_block_data(path: impl AsRef<Path>) -> Result<serde_json::Value> {
    let compressed = std::fs::read(path)?;
    let decompressed = decompress_gzip(&compressed)?;
    Ok(serde_json::from_slice(&decompressed)?)
}

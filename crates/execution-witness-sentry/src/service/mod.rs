use std::sync::Arc;

use lru::LruCache;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::{BlockStorage, ElBlockWitness};

pub mod backfill;
pub mod cl_event;
pub mod el_data;
pub mod el_event;
pub mod proof;

async fn is_el_data_ready(
    block_cache: &Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    storage: &Option<Arc<Mutex<BlockStorage>>>,
    block_hash: &str,
) -> bool {
    {
        let cache = block_cache.lock().await;
        if cache.contains(block_hash) {
            return true;
        }
    }

    if let Some(storage) = &storage {
        let storage_guard = storage.lock().await;
        match storage_guard.load_block_and_witness(block_hash) {
            Ok(Some((block, witness))) => {
                drop(storage_guard);

                let mut cache = block_cache.lock().await;
                cache.put(
                    block_hash.to_string(),
                    ElBlockWitness {
                        block: block.clone(),
                        witness: witness.clone(),
                    },
                );

                debug!(block_hash = %block_hash, "Loaded EL data from disk to cache");
                return true;
            }
            Ok(None) => {
                debug!(block_hash = %block_hash, "EL data not found on disk");
            }
            Err(e) => {
                warn!(block_hash = %block_hash, error = %e, "Failed to load EL data from disk");
            }
        }
    }

    false
}

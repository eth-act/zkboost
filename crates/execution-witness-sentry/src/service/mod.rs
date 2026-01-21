use std::{borrow::Borrow, collections::HashSet, hash::Hash, sync::Arc};

use lru::LruCache;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::{BlockStorage, ElBlockWitness};

pub mod backfill;
pub mod cl_event;
pub mod el_data;
pub mod el_event;
pub mod proof;

#[derive(Clone, Debug)]
pub enum Target<T> {
    All,
    Specific(HashSet<T>),
}

impl<T: Clone + Eq + Hash> FromIterator<T> for Target<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::Specific(iter.into_iter().collect())
    }
}

impl<'a, T: Clone + Eq + Hash> Target<T> {
    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        match self {
            Self::All => true,
            Self::Specific(specific) => specific.contains(value),
        }
    }

    pub fn union(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::Specific(lhs), Self::Specific(rhs)) => {
                Self::Specific(lhs.union(rhs).cloned().collect())
            }
        }
    }

    pub fn filter_by_key<R, Q: ?Sized + Eq + Hash>(
        &'a self,
        iter: impl 'a + IntoIterator<Item = R>,
        f: impl 'a + Fn(&R) -> &Q,
    ) -> impl 'a + Iterator<Item = R>
    where
        T: Borrow<Q>,
    {
        iter.into_iter().filter(move |item| match self {
            Self::All => true,
            Self::Specific(specific) => specific.contains(f(item)),
        })
    }

    pub fn filter(
        &'a self,
        iter: impl 'a + IntoIterator<Item = T>,
    ) -> impl 'a + Iterator<Item = T> {
        self.filter_by_key(iter, |item| item)
    }
}

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

//! Service architecture for the execution-witness-sentry.
//!
//! This module defines the core services that make up the execution-witness-sentry.
//!
//! # Architecture Overview
//!
//! The sentry is composed of services that communicate via channels:
//!
//! ```text
//! ┌────────────────────────────────────────────────────────┐                ┌────────────────────────────────────────────────────┐
//! │                          CLs                           │                │                        ELs                         │
//! └────────────────────────────────────────────────────────┘                └────────────────────────────────────────────────────┘
//!        ▲                 ▲                     ▲                                   ▲                                 ▲
//!        │                 │                     │                                   │                                 │
//!        │                 │                     │                                   │                                 │
//!        │            Listen heads        Get sync status                Fetch block and witness                  Listen heads
//!        │               (SSE)                   │                                   │                            (Websocket)
//!        │                 │                     │                                   │                                 │
//!        │        ┌────────┴─────────┐  ┌────────┴─────────┐                ┌────────┴────────┐               ┌────────┴─────────┐
//!  Submit proofs  │ CL Event Service │  │ Backfill Service ├──Fetch data──► │ EL Data Service │◄──Fetch data──┤ EL Event Service │    
//!        │        └────────┬─────────┘  └────────┬─────────┘                └────────┬────────┘               └──────────────────┘
//!        │                 │                     │                                   │
//!        │                 │                     │                                   │
//!        │           Request proof         Request proof                       Request proof
//!        │                 │                     │                                   │
//!        │                 │                     │                                   │
//!        │                 ▼                     ▼                                   ▼
//! ┌──────┴─────────────────────────────────────────────────────────────────────────────────────┐
//! │                                       Proof Service                                        │
//! └──────┬─────────────────────────────────────────────────────────────────────────────────────┘
//!        │             ▲
//!        │             │
//!  Request proof       │
//!        │        Proof result
//!        │         (Webhook)
//!        ▼             │
//! ┌────────────────────┴───────┐
//! │        Proof Engine        │
//! └────────────────────────────┘
//! ```
//!
//! # Services
//!
//! - [`cl_event::ClEventService`]: Subscribes to CL SSE head events and triggers proof requests
//! - [`el_event::ElEventService`]: Subscribes to EL WebSocket head events and triggers data fetches
//! - [`el_data::ElDataService`]: Fetches block and witness data from EL, caches to memory/disk
//! - [`proof::ProofService`]: Manages proof lifecycle - requesting, receiving, submitting
//! - [`backfill::BackfillService`]: Monitors zkVM CL sync status and backfills missing proofs

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

/// Represents a target selection that can be either all items or a specific subset.
#[derive(Clone, Debug)]
pub enum Target<T> {
    /// Target all items.
    All,
    /// Target only the specified items.
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

/// Checks if EL block and witness is available for the given block hash.
///
/// If data is found on disk but not in cache, it is automatically loaded into the cache
/// for faster subsequent access.
///
/// # Returns
///
/// `true` if the EL block and witness is available (in cache or loaded from disk),
/// `false` otherwise.
pub(crate) async fn is_el_data_available(
    block_cache: &Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    storage: &Option<Arc<Mutex<BlockStorage>>>,
    block_hash: &str,
) -> bool {
    if block_cache.lock().await.contains(block_hash) {
        return true;
    }

    let Some(storage) = &storage else {
        return false;
    };

    let storage_guard = storage.lock().await;
    match storage_guard.load_block_and_witness(block_hash) {
        Ok(Some((block, witness))) => {
            drop(storage_guard);

            let mut cache = block_cache.lock().await;
            cache.put(block_hash.to_string(), ElBlockWitness { block, witness });

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

    false
}

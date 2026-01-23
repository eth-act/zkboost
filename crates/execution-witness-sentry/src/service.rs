//! Service architecture for the execution-witness-sentry.
//!
//! This module defines the core services that make up the execution-witness-sentry.
//!
//! # Architecture Overview
//!
//! The sentry is composed of services that communicate via channels:
//!
//! ```text
//! ┌────────────────────────────────────────────────────────┐                  ┌──────────────────────────────────────────────────────┐
//! │                          CLs                           │                  │                         ELs                          │
//! └────────────────────────────────────────────────────────┘                  └──────────────────────────────────────────────────────┘
//!        ▲                 ▲                     ▲                                     ▲                                   ▲
//!        │                 │                     │                                     │                                   │
//!        │                 │                     │                                     │                                   │
//!        │            Listen heads        Get sync status                  Fetch block and witness                    Listen heads
//!        │               (SSE)                   │                                     │                              (Websocket)
//!        │                 │                     │                                     │                                   │
//!        │        ┌────────┴─────────┐  ┌────────┴─────────┐                  ┌────────┴────────┐                 ┌────────┴─────────┐
//!  Submit proofs  │ CL Event Service │  │ Backfill Service ├──Request data──► │ EL Data Service │◄──Request data──┤ EL Event Service │    
//!        │        └────────┬─────────┘  └────────┬─────────┘                  └────────┬────────┘                 └──────────────────┘
//!        │                 │                     │                                     │
//!        │                 │                     │                                     │
//!        │           Request proof         Request proof                         EL data ready
//!        │                 │                     │                                     │
//!        │                 │                     │                                     │
//!        │                 ▼                     ▼                                     ▼
//! ┌──────┴──────────────────────────────────────────────────────────────────────────────────────┐
//! │                                       Proof Service                                         │
//! └──────┬──────────────────────────────────────────────────────────────────────────────────────┘
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

use crate::{BlockStorage, ElBlockWitness, Hash256};

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

impl<T: Clone + Eq + Hash> Target<T> {
    /// Returns `true` if the target includes the given value.
    ///
    /// For [`Target::All`], always returns `true`. For [`Target::Specific`],
    /// returns `true` only if the value is in the set.
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

    /// Computes the union of two targets.
    ///
    /// If either target is [`Target::All`], the result is [`Target::All`].
    /// Otherwise, returns a [`Target::Specific`] containing items from both sets.
    pub fn union(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::Specific(lhs), Self::Specific(rhs)) => {
                Self::Specific(lhs.union(rhs).cloned().collect())
            }
        }
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
    el_data_cache: &Arc<Mutex<LruCache<Hash256, ElBlockWitness>>>,
    storage: &Option<Arc<Mutex<BlockStorage>>>,
    block_hash: Hash256,
) -> bool {
    if el_data_cache.lock().await.contains(&block_hash) {
        return true;
    }

    let Some(storage) = &storage else {
        return false;
    };

    let storage_guard = storage.lock().await;
    match storage_guard.load_el_data(block_hash) {
        Ok(Some(el_data)) => {
            drop(storage_guard);

            let mut cache = el_data_cache.lock().await;
            cache.put(block_hash, el_data);

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

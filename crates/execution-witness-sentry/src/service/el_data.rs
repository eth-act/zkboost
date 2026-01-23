//! EL Data Service
//!
//! This module provides [`ElDataService`], which is responsible for fetching block and witness
//! data from EL clients.
//!
//! ## Purpose
//!
//! The EL data is necessary for proof generation, when a new EL block event arrives, or a proof
//! request is processed but the EL data is missing, a [`ElDataServiceMessage::FetchData`] will be
//! sent to this service to fetch the data. It deduplicates the messages with same `block_hash` to
//! prevent duplicate concurrent requests for the same block.

use std::{collections::HashSet, sync::Arc};

use lru::LruCache;
use tokio::{
    sync::{Mutex, mpsc},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    BlockStorage, ElBlockWitness, ElClient, Hash256,
    service::{is_el_data_available, proof::ProofServiceMessage},
};

/// Messages handled by [`ElDataService`].
pub enum ElDataServiceMessage {
    /// Request to fetch block and witness data for the given block hash.
    ///
    /// Sent when a new EL block event arrives or when proof generation
    /// requires data that is not yet available.
    FetchData { block_hash: Hash256 },
}

/// Service responsible for fetching block and witness data from EL clients.
///
/// When a [`ElDataServiceMessage::FetchData`] message is received, this service
/// fetches the block and execution witness from configured EL endpoints. Successfully
/// fetched data is cached in memory and optionally persisted to disk, then a
/// [`ProofServiceMessage::BlockDataReady`] notification is sent to the proof service.
///
/// ## Deduplication
///
/// Concurrent requests for the same block hash are deduplicated via the `in_flight`
/// set to prevent redundant network calls.
pub struct ElDataService {
    /// EL clients to fetch block data from, tried in order.
    el_clients: Vec<Arc<ElClient>>,
    /// In-memory LRU cache for recently fetched EL block data.
    el_data_cache: Arc<Mutex<LruCache<Hash256, ElBlockWitness>>>,
    /// Optional disk storage for persisting fetched data.
    storage: Option<Arc<Mutex<BlockStorage>>>,
    /// Channel to notify the proof service when data is ready.
    proof_tx: mpsc::Sender<ProofServiceMessage>,
    /// Set of block hashes currently being fetched, used for deduplication.
    in_flight: Arc<Mutex<HashSet<Hash256>>>,
}

impl ElDataService {
    /// Creates a new EL data service.
    pub fn new(
        el_clients: Vec<Arc<ElClient>>,
        el_data_cache: Arc<Mutex<LruCache<Hash256, ElBlockWitness>>>,
        storage: Option<Arc<Mutex<BlockStorage>>>,
        proof_tx: mpsc::Sender<ProofServiceMessage>,
    ) -> Self {
        Self {
            el_clients,
            el_data_cache,
            storage,
            proof_tx,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Spawns the service as a background task.
    pub fn spawn(
        self: Arc<Self>,
        shutdown_token: CancellationToken,
        el_data_rx: mpsc::Receiver<ElDataServiceMessage>,
    ) -> JoinHandle<()> {
        tokio::spawn(self.run(shutdown_token, el_data_rx))
    }

    /// Main event loop that processes incoming messages until shutdown.
    async fn run(
        self: Arc<Self>,
        shutdown_token: CancellationToken,
        mut el_data_rx: mpsc::Receiver<ElDataServiceMessage>,
    ) {
        let mut fetch_tasks: JoinSet<Hash256> = JoinSet::new();

        loop {
            tokio::select! {
                biased;

                _ = shutdown_token.cancelled() => {
                    info!("ElDataService received shutdown signal");
                    fetch_tasks.abort_all();
                    break;
                }

                Some(result) = fetch_tasks.join_next() => {
                    match result {
                        Ok(block_hash) => {
                            self.in_flight.lock().await.remove(&block_hash);
                        },
                        Err(e) if e.is_panic() => error!(error = ?e, "Fetch task panicked"),
                        Err(_) => {}
                    }
                }

                Some(message) = el_data_rx.recv() => {
                    match message {
                        ElDataServiceMessage::FetchData { block_hash } => {
                            self.handle_fetch_data(block_hash, &mut fetch_tasks).await;
                        }
                    }
                }
            }
        }
    }

    /// Handles a fetch data request, spawning a fetch task if not already in flight.
    async fn handle_fetch_data(
        self: &Arc<Self>,
        block_hash: Hash256,
        fetch_tasks: &mut JoinSet<Hash256>,
    ) {
        if is_el_data_available(&self.el_data_cache, &self.storage, block_hash).await {
            self.send_block_data_ready(block_hash).await;
            return;
        };

        if !self.in_flight.lock().await.insert(block_hash) {
            debug!(block_hash = %block_hash, "Block fetch already in flight, skipping");
            return;
        }

        fetch_tasks.spawn(self.clone().fetch_el_data(block_hash));
    }

    /// Fetches EL block and witness, caching and persisting and notifying if success.
    async fn fetch_el_data(self: Arc<Self>, block_hash: Hash256) -> Hash256 {
        for el_client in &self.el_clients {
            let block = match el_client.get_block_by_hash(block_hash).await {
                Ok(Some(data)) => data,
                Ok(None) => {
                    debug!(block_hash = %block_hash, "Block not found on EL");
                    continue;
                }
                Err(e) => {
                    warn!(block_hash = %block_hash, error = %e, "Failed to fetch block from EL");
                    continue;
                }
            };

            let witness = match el_client.get_execution_witness_by_hash(block_hash).await {
                Ok(Some(data)) => data,
                Ok(None) => {
                    debug!(block_hash = %block_hash, "Witness not found on EL");
                    continue;
                }
                Err(e) => {
                    warn!(block_hash = %block_hash, error = %e, "Failed to fetch witness from EL");
                    continue;
                }
            };

            let block_number = block.header.number;
            let el_data = ElBlockWitness { block, witness };

            info!(
                block_number = block_number,
                block_hash = %block_hash,
                "Fetched block and witness from EL"
            );

            if let Some(storage) = &self.storage {
                let mut storage_guard = storage.lock().await;
                if let Err(e) = storage_guard.save_el_data(&el_data) {
                    warn!(block_hash = %block_hash, error = %e, "Failed to save fetched block to disk");
                } else {
                    debug!(
                        block_number = block_number,
                        block_hash = %block_hash,
                        "Saved fetched block to disk"
                    );
                }
            }

            let mut cache = self.el_data_cache.lock().await;
            cache.put(block_hash, el_data);
            debug!(block_hash = %block_hash, "Cached fetched block in memory");

            self.send_block_data_ready(block_hash).await;

            return block_hash;
        }

        error!(
            block_hash = %block_hash,
            "Failed to fetch block and witness from any EL"
        );

        block_hash
    }

    /// Notifies the proof service that block data is available.
    async fn send_block_data_ready(&self, block_hash: Hash256) {
        let msg = ProofServiceMessage::BlockDataReady { block_hash };
        if let Err(e) = self.proof_tx.send(msg).await {
            error!(error = %e, "Failed to send block ready notification");
        }
    }
}

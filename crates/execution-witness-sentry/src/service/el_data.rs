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
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    BlockStorage, ElBlockWitness, ElClient,
    service::{is_el_data_available, proof::ProofServiceMessage},
};

pub enum ElDataServiceMessage {
    FetchData { block_hash: String },
}

pub struct ElDataService {
    el_clients: Vec<Arc<ElClient>>,
    block_cache: Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    storage: Option<Arc<Mutex<BlockStorage>>>,
    el_data_rx: mpsc::Receiver<ElDataServiceMessage>,
    proof_tx: mpsc::Sender<ProofServiceMessage>,
    /// Tracks block hashes currently being fetched to prevent duplicate concurrent requests.
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl ElDataService {
    pub fn new(
        el_clients: Vec<Arc<ElClient>>,
        block_cache: Arc<Mutex<LruCache<String, ElBlockWitness>>>,
        storage: Option<Arc<Mutex<BlockStorage>>>,
        el_data_rx: mpsc::Receiver<ElDataServiceMessage>,
        proof_tx: mpsc::Sender<ProofServiceMessage>,
    ) -> Self {
        Self {
            el_clients,
            block_cache,
            storage,
            el_data_rx,
            proof_tx,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub async fn run(mut self, shutdown_token: CancellationToken) {
        let mut fetch_tasks: JoinSet<String> = JoinSet::new();

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

                Some(message) = self.el_data_rx.recv() => {
                    match message {
                        ElDataServiceMessage::FetchData { block_hash } => {
                            self.handle_fetch_data(block_hash, &mut fetch_tasks).await;
                        }
                    }
                }
            }
        }
    }

    async fn handle_fetch_data(&self, block_hash: String, fetch_tasks: &mut JoinSet<String>) {
        if is_el_data_available(&self.block_cache, &self.storage, &block_hash).await {
            let msg = ProofServiceMessage::BlockDataReady {
                block_hash: block_hash.clone(),
            };
            if let Err(e) = self.proof_tx.send(msg).await {
                error!(error = %e, "Failed to send block ready notification");
            }
        };

        if !self.in_flight.lock().await.insert(block_hash.clone()) {
            debug!(block_hash = %block_hash, "Block fetch already in flight, skipping");
            return;
        }

        let block_cache_clone = self.block_cache.clone();
        let storage_clone = self.storage.clone();
        let proof_tx_clone = self.proof_tx.clone();
        let el_clients_clone = self.el_clients.clone();

        fetch_tasks.spawn(async move {
            let result = fetch_block_from_el(
                &el_clients_clone,
                &block_cache_clone,
                &storage_clone,
                &block_hash,
            )
            .await;

            if result.is_ok() {
                let msg = ProofServiceMessage::BlockDataReady {
                    block_hash: block_hash.clone(),
                };
                if let Err(e) = proof_tx_clone.send(msg).await {
                    error!(error = %e, "Failed to send block ready notification");
                }
            }

            block_hash
        });
    }
}

async fn fetch_block_from_el(
    el_clients: &[Arc<ElClient>],
    block_cache: &Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    storage: &Option<Arc<Mutex<BlockStorage>>>,
    block_hash: &str,
) -> anyhow::Result<()> {
    for el_client in el_clients {
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

        if let Some(storage) = storage {
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

        let mut cache = block_cache.lock().await;
        cache.put(block_hash.to_string(), el_data);
        debug!(block_hash = %block_hash, "Cached fetched block in memory");

        return Ok(());
    }

    Err(anyhow::anyhow!(
        "Failed to fetch block from any EL endpoint"
    ))
}

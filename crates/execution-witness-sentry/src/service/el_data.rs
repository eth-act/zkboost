use std::{collections::HashSet, sync::Arc};

use lru::LruCache;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{BlockStorage, ElBlockWitness, ElClient, service::proof::ProofServiceMessage};

pub enum ElDataServiceMessage {
    FetchData { block_hash: String },
}

pub struct ElDataService {
    el_clients: Vec<Arc<ElClient>>,
    block_cache: Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    storage: Option<Arc<Mutex<BlockStorage>>>,
    el_data_rx: mpsc::Receiver<ElDataServiceMessage>,
    proof_tx: mpsc::Sender<ProofServiceMessage>,
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
        }
    }

    pub async fn run(mut self, shutdown_token: CancellationToken) {
        let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        loop {
            tokio::select! {
                biased;

                _ = shutdown_token.cancelled() => {
                    info!("ElDataService received shutdown signal");
                    break;
                }

                Some(message) = self.el_data_rx.recv() => {
                    match message {
                        ElDataServiceMessage::FetchData { block_hash } => {
                            self.handle_fetch_data(block_hash, &in_flight).await;
                        }
                    }
                }
            }
        }
    }

    async fn handle_fetch_data(&self, block_hash: String, in_flight: &Arc<Mutex<HashSet<String>>>) {
        {
            let in_flight_guard = in_flight.lock().await;
            if in_flight_guard.contains(&block_hash) {
                debug!(block_hash = %block_hash, "Block fetch already in flight, skipping");
                return;
            }
        }

        {
            let cache = self.block_cache.lock().await;
            if cache.contains(&block_hash) {
                debug!(block_hash = %block_hash, "Block already in cache");
                if let Err(e) = self
                    .proof_tx
                    .send(ProofServiceMessage::BlockDataReady {
                        block_hash: block_hash.clone(),
                    })
                    .await
                {
                    error!(error = %e, "Failed to send block ready notification");
                }
                return;
            }
        }

        if let Some(ref storage) = self.storage {
            let storage_guard = storage.lock().await;
            match storage_guard.load_block_and_witness(&block_hash) {
                Ok(Some((block, witness))) => {
                    drop(storage_guard);

                    let mut cache = self.block_cache.lock().await;
                    cache.put(
                        block_hash.clone(),
                        ElBlockWitness {
                            block: block.clone(),
                            witness: witness.clone(),
                        },
                    );
                    drop(cache);

                    debug!(block_hash = %block_hash, "Loaded block from disk to cache");

                    if let Err(e) = self
                        .proof_tx
                        .send(ProofServiceMessage::BlockDataReady {
                            block_hash: block_hash.clone(),
                        })
                        .await
                    {
                        error!(error = %e, "Failed to send block ready notification");
                    }
                    return;
                }
                Ok(None) => {
                    debug!(block_hash = %block_hash, "Block not found on disk");
                }
                Err(e) => {
                    warn!(block_hash = %block_hash, error = %e, "Failed to load block from disk");
                }
            }
        }

        {
            let mut in_flight_guard = in_flight.lock().await;
            in_flight_guard.insert(block_hash.clone());
        }

        let in_flight_clone = in_flight.clone();
        let block_cache_clone = self.block_cache.clone();
        let storage_clone = self.storage.clone();
        let proof_tx_clone = self.proof_tx.clone();
        let el_clients_clone = self.el_clients.clone();

        tokio::spawn(async move {
            let result = fetch_block_from_el(
                &block_hash,
                &el_clients_clone,
                &block_cache_clone,
                &storage_clone,
            )
            .await;

            {
                let mut in_flight_guard = in_flight_clone.lock().await;
                in_flight_guard.remove(&block_hash);
            }

            if result.is_ok()
                && let Err(e) = proof_tx_clone
                    .send(ProofServiceMessage::BlockDataReady {
                        block_hash: block_hash.clone(),
                    })
                    .await
            {
                error!(error = %e, "Failed to send block ready notification");
            }
        });
    }
}

async fn fetch_block_from_el(
    block_hash: &str,
    el_clients: &[Arc<ElClient>],
    block_cache: &Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    storage: &Option<Arc<Mutex<BlockStorage>>>,
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
            if let Err(e) = storage_guard.save_block(&el_data) {
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

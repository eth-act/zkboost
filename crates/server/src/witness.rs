//! Witness fetching service.
//!
//! This module provides `WitnessService`, which is responsible for fetching execution witness
//! data from the EL client. It responds to `WitnessServiceMessage::FetchWitness` requests from
//! the proof service, retrying failed fetches every second until success or timeout.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use lru::LruCache;
use stateless::ExecutionWitness;
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::interval,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use zkboost_types::Hash256;

use crate::{el_client::ElClient, proof::ProofServiceMessage};

const CLEANUP_INTERVAL: Duration = Duration::from_secs(12);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Messages consumed by the witness service event loop.
#[derive(Debug)]
pub(crate) enum WitnessServiceMessage {
    /// Request to fetch the execution witness for the given block hash.
    FetchWitness { block_hash: Hash256 },
}

/// Fetches execution witness data from the EL client on demand.
#[allow(missing_debug_implementations)]
pub(crate) struct WitnessService {
    el_client: Arc<ElClient>,
    proof_service_tx: mpsc::Sender<ProofServiceMessage>,
    witness_timeout: Duration,
    witness_cache_size: usize,
}

impl WitnessService {
    /// Creates a new witness service with the given EL client and proof sender.
    pub(crate) fn new(
        el_client: Arc<ElClient>,
        proof_service_tx: mpsc::Sender<ProofServiceMessage>,
        witness_timeout: Duration,
        witness_cache_size: usize,
    ) -> Self {
        Self {
            el_client,
            proof_service_tx,
            witness_timeout,
            witness_cache_size,
        }
    }

    /// Spawns the witness service event loop as a background task.
    pub(crate) fn spawn(
        self,
        shutdown_token: CancellationToken,
        witness_service_rx: mpsc::Receiver<WitnessServiceMessage>,
    ) -> JoinHandle<()> {
        tokio::spawn(self.run(shutdown_token, witness_service_rx))
    }

    async fn run(
        self,
        shutdown_token: CancellationToken,
        mut witness_service_rx: mpsc::Receiver<WitnessServiceMessage>,
    ) {
        let mut witness_cache: LruCache<Hash256, Arc<ExecutionWitness>> = LruCache::new(
            NonZeroUsize::new(self.witness_cache_size)
                .expect("witness_cache_size must be non-zero"),
        );
        let mut unresolved: HashMap<Hash256, Instant> = HashMap::new();
        let mut in_flight: HashSet<Hash256> = HashSet::new();
        let mut tasks: JoinSet<(Hash256, Option<ExecutionWitness>)> = JoinSet::new();

        let mut cleanup_interval = interval(CLEANUP_INTERVAL);
        let mut retry_interval = interval(RETRY_INTERVAL);

        loop {
            tokio::select! {
                biased;

                _ = shutdown_token.cancelled() => {
                    info!("witness service shutting down");
                    tasks.abort_all();
                    break;
                }

                Some(result) = tasks.join_next() => {
                    match result {
                        Ok((block_hash, Some(witness))) => {
                            let witness = Arc::new(witness);
                            witness_cache.put(block_hash, witness.clone());
                            unresolved.remove(&block_hash);
                            in_flight.remove(&block_hash);
                            info!(block_hash = %block_hash, "witness fetched and cached");
                            if let Err(error) = self.proof_service_tx.send(ProofServiceMessage::WitnessAvailable { block_hash, witness }).await {
                                error!(error = %error, "witness ready send failed");
                            }
                        }
                        Ok((block_hash, None)) => {
                            in_flight.remove(&block_hash);
                        }
                        Err(error) if error.is_panic() => {
                            error!(error = ?error, "fetch task panicked");
                        }
                        Err(_) => {}
                    }
                }

                Some(message) = witness_service_rx.recv() => {
                    match message {
                        WitnessServiceMessage::FetchWitness { block_hash } => {
                            if let Some(witness) = witness_cache.peek(&block_hash).cloned() {
                                debug!(block_hash = %block_hash, "witness cache hit");
                                if let Err(error) = self.proof_service_tx.send(ProofServiceMessage::WitnessAvailable { block_hash, witness }).await {
                                    error!(error = %error, "witness ready send failed");
                                }
                                continue;
                            }

                            if in_flight.contains(&block_hash) {
                                debug!(block_hash = %block_hash, "witness fetch in flight");
                                continue;
                            }

                            unresolved.entry(block_hash).or_insert_with(Instant::now);
                            in_flight.insert(block_hash);
                            let el_client = self.el_client.clone();
                            tasks.spawn(fetch_witness_task(el_client, block_hash));
                        }
                    }
                }

                // Re-dispatch all unresolved witnesses that are not currently in flight.
                _ = retry_interval.tick() => {
                    for &block_hash in unresolved.keys() {
                        if !in_flight.contains(&block_hash) {
                            in_flight.insert(block_hash);
                            let el_client = self.el_client.clone();
                            tasks.spawn(fetch_witness_task(el_client, block_hash));
                        }
                    }
                }

                _ = cleanup_interval.tick() => {
                    let timeout = self.witness_timeout;
                    unresolved.retain(|block_hash, created_at| {
                        let is_stale = created_at.elapsed() >= timeout;
                        if is_stale {
                            warn!(
                                block_hash = %block_hash,
                                elapsed_secs = created_at.elapsed().as_secs(),
                                "pending fetch timed out"
                            );
                        }
                        !is_stale
                    });
                }
            }
        }
    }
}

async fn fetch_witness_task(
    el_client: Arc<ElClient>,
    block_hash: Hash256,
) -> (Hash256, Option<ExecutionWitness>) {
    match el_client.get_execution_witness_by_hash(block_hash).await {
        Ok(Some(witness)) => (block_hash, Some(witness)),
        Ok(None) => {
            debug!(block_hash = %block_hash, "witness not found");
            (block_hash, None)
        }
        Err(error) => {
            warn!(block_hash = %block_hash, error = %error, "witness fetch failed");
            (block_hash, None)
        }
    }
}

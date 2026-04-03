//! Witness fetching service.
//!
//! This module provides `WitnessService`, which is responsible for fetching execution witness data
//! from the EL client. It responds to `WitnessServiceMessage::FetchWitness` requests from the proof
//! service. Each fetch is a self-contained task that retries until success or the configured
//! witness timeout elapses.

use std::{
    collections::HashSet, num::NonZeroUsize, panic::AssertUnwindSafe, sync::Arc, time::Duration,
};

use futures::FutureExt;
use lru::LruCache;
use stateless::ExecutionWitness;
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use zkboost_types::Hash256;

use crate::{el_client::ElClient, proof::ProofServiceMessage};

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
    witness_cache: LruCache<Hash256, Arc<ExecutionWitness>>,
}

type TaskResult = (Hash256, Option<Arc<ExecutionWitness>>);

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
            witness_cache: LruCache::new(
                NonZeroUsize::new(witness_cache_size).expect("witness_cache_size must be non-zero"),
            ),
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
        mut self,
        shutdown_token: CancellationToken,
        mut witness_service_rx: mpsc::Receiver<WitnessServiceMessage>,
    ) {
        let mut requested = HashSet::new();
        let mut tasks: JoinSet<TaskResult> = JoinSet::new();

        loop {
            tokio::select! {
                biased;

                _ = shutdown_token.cancelled() => {
                    info!("witness service shutting down");
                    tasks.abort_all();
                    break;
                }

                Some(result) = tasks.join_next() => {
                    if let Ok((block_hash, witness)) = result {
                        self.handle_task_result(block_hash, witness, &mut requested)
                            .await;
                    }
                }

                Some(msg) = witness_service_rx.recv() => {
                    self.handle_message(msg, &mut requested, &mut tasks).await;
                }
            }
        }
    }

    async fn handle_task_result(
        &mut self,
        block_hash: Hash256,
        witness: Option<Arc<ExecutionWitness>>,
        requested: &mut HashSet<Hash256>,
    ) {
        requested.remove(&block_hash);
        match witness {
            Some(witness) => {
                self.witness_cache.put(block_hash, witness.clone());
                info!(%block_hash, "witness fetched and cached");
                if let Err(error) = self
                    .proof_service_tx
                    .send(ProofServiceMessage::WitnessAvailable {
                        block_hash,
                        witness,
                    })
                    .await
                {
                    error!(%error, "witness available send failed");
                }
            }
            None => {
                warn!(%block_hash, "witness fetch timed out");
                if let Err(error) = self
                    .proof_service_tx
                    .send(ProofServiceMessage::WitnessTimeout { block_hash })
                    .await
                {
                    error!(%error, "witness timeout send failed");
                }
            }
        }
    }

    async fn handle_message(
        &self,
        message: WitnessServiceMessage,
        requested: &mut HashSet<Hash256>,
        tasks: &mut JoinSet<TaskResult>,
    ) {
        match message {
            WitnessServiceMessage::FetchWitness { block_hash } => {
                if let Some(witness) = self.witness_cache.peek(&block_hash).cloned() {
                    debug!(%block_hash, "witness cache hit");
                    if let Err(error) = self
                        .proof_service_tx
                        .send(ProofServiceMessage::WitnessAvailable {
                            block_hash,
                            witness,
                        })
                        .await
                    {
                        error!(%error, "witness available send failed");
                    }
                    return;
                }

                if !requested.insert(block_hash) {
                    debug!(%block_hash, "witness already requested");
                    return;
                }

                tasks.spawn(fetch_witness(
                    self.el_client.clone(),
                    block_hash,
                    self.witness_timeout,
                ));
            }
        }
    }
}

async fn fetch_witness(
    el_client: Arc<ElClient>,
    block_hash: Hash256,
    witness_timeout: Duration,
) -> (Hash256, Option<Arc<ExecutionWitness>>) {
    const RETRY_INTERVAL: Duration = Duration::from_millis(200);
    let fut = async {
        loop {
            let deadline = Instant::now() + RETRY_INTERVAL;
            match el_client.get_execution_witness_by_hash(block_hash).await {
                Ok(Some(witness)) => return Arc::new(witness),
                Ok(None) => debug!(%block_hash, "witness not found, retrying"),
                Err(error) => warn!(%block_hash, %error, "witness fetch failed, retrying"),
            }
            sleep_until(deadline).await;
        }
    };
    match timeout(witness_timeout, AssertUnwindSafe(fut).catch_unwind()).await {
        Ok(Ok(witness)) => (block_hash, Some(witness)),
        _ => (block_hash, None),
    }
}

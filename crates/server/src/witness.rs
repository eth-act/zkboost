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
use tracing::{Instrument, Span, debug, error, info, info_span, record_all, trace, warn};
use zkboost_types::Hash256;

use crate::{dashboard::DashboardMessage, el_client::ElClient, proof::ProofServiceMessage};

/// Messages consumed by the witness service event loop.
#[derive(Debug)]
pub(crate) enum WitnessServiceMessage {
    /// Request to fetch the execution witness for the given block hash.
    FetchWitness { block_hash: Hash256, span: Span },
}

/// Fetches execution witness data from the EL client on demand.
pub(crate) struct WitnessService {
    el_client: Arc<ElClient>,
    proof_service_tx: mpsc::Sender<ProofServiceMessage>,
    dashboard_service_tx: mpsc::Sender<DashboardMessage>,
    witness_timeout: Duration,
    witness_cache: LruCache<Hash256, Arc<ExecutionWitness>>,
    requested: HashSet<Hash256>,
    tasks: JoinSet<TaskResult>,
}

type TaskResult = (Hash256, Option<(Arc<ExecutionWitness>, usize)>);

impl WitnessService {
    /// Creates a new witness service with the given EL client and proof sender.
    pub(crate) fn new(
        el_client: Arc<ElClient>,
        proof_service_tx: mpsc::Sender<ProofServiceMessage>,
        dashboard_service_tx: mpsc::Sender<DashboardMessage>,
        witness_timeout: Duration,
        witness_cache_size: usize,
    ) -> Self {
        Self {
            el_client,
            proof_service_tx,
            dashboard_service_tx,
            witness_timeout,
            witness_cache: LruCache::new(
                NonZeroUsize::new(witness_cache_size).expect("witness_cache_size must be non-zero"),
            ),
            requested: HashSet::new(),
            tasks: JoinSet::new(),
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
        loop {
            tokio::select! {
                biased;

                _ = shutdown_token.cancelled() => {
                    info!("witness service shutting down");
                    self.tasks.abort_all();
                    break;
                }

                Some(result) = self.tasks.join_next() => {
                    if let Ok((block_hash, witness)) = result {
                        self.handle_task_result(block_hash, witness).await;
                    }
                }

                Some(msg) = witness_service_rx.recv() => {
                    self.handle_message(msg).await;
                }
            }
        }
    }

    async fn handle_task_result(
        &mut self,
        block_hash: Hash256,
        witness: Option<(Arc<ExecutionWitness>, usize)>,
    ) {
        self.requested.remove(&block_hash);
        match witness {
            Some((witness, witness_size)) => {
                self.witness_cache.put(block_hash, witness.clone());

                info!(%block_hash, "fetched witness");

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

                let _ = self
                    .dashboard_service_tx
                    .try_send(DashboardMessage::fetch_witness_end(
                        block_hash,
                        witness_size,
                        true,
                    ));
            }
            None => {
                error!(%block_hash, "fetching witness timed out");

                if let Err(error) = self
                    .proof_service_tx
                    .send(ProofServiceMessage::WitnessTimeout { block_hash })
                    .await
                {
                    error!(%error, "witness timeout send failed");
                }

                let _ = self
                    .dashboard_service_tx
                    .try_send(DashboardMessage::fetch_witness_end(block_hash, 0, false));
            }
        }
    }

    async fn handle_message(&mut self, message: WitnessServiceMessage) {
        match message {
            WitnessServiceMessage::FetchWitness { block_hash, span } => {
                trace!(%block_hash, "received WitnessServiceMessage::FetchWitness");

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

                if !self.requested.insert(block_hash) {
                    debug!(%block_hash, "witness already requested");
                    return;
                }

                self.tasks.spawn(fetch_witness(
                    self.el_client.clone(),
                    self.dashboard_service_tx.clone(),
                    block_hash,
                    self.witness_timeout,
                    span,
                ));
            }
        }
    }
}

async fn fetch_witness(
    el_client: Arc<ElClient>,
    dashboard_service_tx: mpsc::Sender<DashboardMessage>,
    block_hash: Hash256,
    witness_timeout: Duration,
    span: Span,
) -> TaskResult {
    info!(%block_hash, "fetching witness");

    let _ = dashboard_service_tx.try_send(DashboardMessage::fetch_witness_start(block_hash));

    let span = info_span!(
        parent: span,
        "fetch_witness",
        otel.status_code = tracing::field::Empty,
        error_reason = tracing::field::Empty,
    );

    const RETRY_INTERVAL: Duration = Duration::from_millis(200);
    let fut = async {
        loop {
            let deadline = Instant::now() + RETRY_INTERVAL;
            match el_client.get_execution_witness_by_hash(block_hash).await {
                Ok(Some(witness_and_size)) => return witness_and_size,
                Ok(None) => debug!(%block_hash, "witness not found, retrying"),
                Err(error) => warn!(%block_hash, %error, "witness fetch failed, retrying"),
            }
            sleep_until(deadline).await;
        }
    }
    .instrument(span.clone());

    match timeout(witness_timeout, AssertUnwindSafe(fut).catch_unwind()).await {
        Ok(Ok((witness, witness_size))) => (block_hash, Some((Arc::new(witness), witness_size))),
        Ok(Err(_)) => {
            record_all!(span, otel.status_code = "ERROR", error_reason = "panic");
            (block_hash, None)
        }
        Err(_) => {
            record_all!(span, otel.status_code = "ERROR", error_reason = "timeout");
            (block_hash, None)
        }
    }
}

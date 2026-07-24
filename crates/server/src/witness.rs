//! Witness fetching service.
//!
//! This module provides `WitnessService`, which is responsible for fetching execution witness data
//! from the EL client. It responds to `WitnessServiceMessage::FetchWitness` requests from the proof
//! service. Each fetch is a self-contained task that retries until success or the configured
//! witness timeout elapses.

use std::{
    collections::HashSet, num::NonZeroUsize, panic::AssertUnwindSafe, sync::Arc, time::Duration,
};

use alloy_primitives::Bytes;
use alloy_rpc_types_debug::ExecutionWitness as AlloyExecutionWitness;
use ere_guests_stateless_validator_common::guest::input::ExecutionWitness;
use futures::FutureExt;
use lru::LruCache;
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, debug, error, info, info_span, record_all, trace, warn};
use zkboost_types::{Hash256, SszList};

use crate::{
    dashboard::DashboardMessage, el_client::ElClient, metrics::record_witness_fetch,
    proof::ProofServiceMessage,
};

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

type TaskResult = (Hash256, Option<(AlloyExecutionWitness, usize)>);

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
        witness: Option<(AlloyExecutionWitness, usize)>,
    ) {
        self.requested.remove(&block_hash);
        match witness {
            Some((witness, witness_size)) => {
                match from_rpc_witness(witness) {
                    Ok(witness) => {
                        let witness = Arc::new(witness);
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

                        let _ = self.dashboard_service_tx.try_send(
                            DashboardMessage::fetch_witness_end(block_hash, witness_size, true),
                        );
                    }
                    Err(error) => {
                        error!(%error, "incompatible with SSZ container limit");

                        if let Err(send_error) = self
                            .proof_service_tx
                            .send(ProofServiceMessage::WitnessIncompatible {
                                block_hash,
                                error: error.to_string(),
                            })
                            .await
                        {
                            error!(%send_error, "witness incompatible send failed");
                        }
                        let _ = self.dashboard_service_tx.try_send(
                            DashboardMessage::fetch_witness_end(block_hash, witness_size, false),
                        );
                    }
                };
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
        parent: &span,
        "fetch_witness",
        // The witness service is keyed by execution block hash and serves all
        // requests for that block, so the hash — not a request root — is its
        // identity.
        block_hash = %block_hash,
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

    let fetch_start = Instant::now();
    match timeout(witness_timeout, AssertUnwindSafe(fut).catch_unwind()).await {
        Ok(Ok((witness, witness_size))) => {
            record_witness_fetch("success", fetch_start.elapsed(), witness_size);
            (block_hash, Some((witness, witness_size)))
        }
        Ok(Err(_)) => {
            record_witness_fetch("panic", fetch_start.elapsed(), 0);
            record_all!(span, otel.status_code = "ERROR", error_reason = "panic");
            (block_hash, None)
        }
        Err(_) => {
            record_witness_fetch("timeout", fetch_start.elapsed(), 0);
            record_all!(span, otel.status_code = "ERROR", error_reason = "timeout");
            (block_hash, None)
        }
    }
}

/// Converts the RPC debug execution witness into the `ExecutionWitness`.
fn from_rpc_witness(value: AlloyExecutionWitness) -> anyhow::Result<ExecutionWitness> {
    Ok(ExecutionWitness {
        state: ssz_bytes_list(value.state, "witness state")?,
        codes: ssz_bytes_list(value.codes, "witness codes")?,
        headers: ssz_bytes_list(value.headers, "witness headers")?,
    })
}

fn ssz_bytes_list<const M: usize, const N: usize>(
    items: Vec<Bytes>,
    label: &str,
) -> anyhow::Result<SszList<SszList<u8, M>, N>> {
    let list = items
        .into_iter()
        .map(|item| SszList::try_from(Vec::from(item)))
        .collect::<Result<_, _>>()
        .map_err(|err| anyhow::anyhow!("{label} item length should be within bounds: {err:?}"))?;
    ssz_list(list, label)
}

fn ssz_list<T, const N: usize>(values: Vec<T>, label: &str) -> anyhow::Result<SszList<T, N>> {
    SszList::try_from(values)
        .map_err(|err| anyhow::anyhow!("{label} length should be within bounds: {err:?}"))
}

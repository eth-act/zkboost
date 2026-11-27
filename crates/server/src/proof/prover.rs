//! The provers, one per configured proof type. A prover is a remote ere-server, an in-process
//! mock, or a remote ZisK or OpenVM cluster, and proves one payload at a time in the order of
//! arrival.

use std::{
    collections::VecDeque,
    future::Future,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{Context, ensure};
use bytes::Bytes;
use ere_catalog::zkVMKind;
use ere_server_client::{Input, OtelPropagation, zkVMClient};
use rand::{Rng, rng};
use stateless_validator_common::SszEncode;
use tokio::{
    fs,
    sync::{
        OnceCell,
        mpsc::{self, UnboundedReceiver, UnboundedSender},
    },
    time::{sleep, timeout},
};
use tokio_util::task::TaskTracker;
use tracing::{error, info, warn};

use crate::{
    beacon::execution_proof::{MAX_PROOF_SIZE, MockProof},
    config::{Backend as BackendConfig, MockProvingTime, ZkvmConfig},
    engine::payload::ProofInput,
    observability::metrics::{DispatchFailure, ProverMetrics},
    proof::{
        proof_type::ProofType,
        prover::{openvm::OpenvmCluster, zisk::ZiskCluster},
    },
    util::RETRY_DELAY,
};

mod openvm;
mod zisk;

/// Proofs that can wait for one prover, which covers a short reorg. A proof that arrives at a
/// full queue takes the place of the oldest one, which is not proven.
const QUEUE_CAPACITY: usize = 8;

const ELF_FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Bound on the cancel of a remote job, which a shutdown waits for.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Prover {
    pub(crate) proof_type: ProofType,
    pub(crate) timeout: Duration,
    pub(crate) metrics: ProverMetrics,
    config: BackendConfig,
    /// The guest ELF of a cluster, loaded once.
    elf: OnceCell<Vec<u8>>,
    /// Set once the backend is connected.
    backend: OnceLock<Backend>,
    job_tx: UnboundedSender<Job>,
    /// Runs the cancel of a remote job, so a shutdown waits for it.
    tasks: TaskTracker,
}

/// A proof that waits for its prover. The prover calls it with the turn of the proof.
pub(crate) type Job = Box<dyn FnOnce(Turn) + Send>;

/// The turn of the one proof that a prover proves. The prover takes the next proof when the
/// turn drops.
pub(crate) struct Turn(UnboundedSender<()>);

impl Drop for Turn {
    fn drop(&mut self) {
        // A prover that stopped does not read the end of a turn.
        self.0.send(()).ok();
    }
}

enum Backend {
    Ere(zkVMClient),
    Mock,
    ZiskCluster(ZiskCluster),
    OpenvmCluster(Arc<OpenvmCluster>),
}

impl Prover {
    /// Returns the prover and `job_rx`, the receiver of its queue, which `run_queue` takes.
    pub(crate) fn new(config: &ZkvmConfig, tasks: TaskTracker) -> (Self, UnboundedReceiver<Job>) {
        let (job_tx, job_rx) = mpsc::unbounded_channel();
        let prover = Self {
            proof_type: config.proof_type,
            timeout: Duration::from_secs(config.proof_timeout_secs.get()),
            metrics: ProverMetrics::new(config.proof_type),
            config: config.backend.clone(),
            elf: OnceCell::new(),
            backend: OnceLock::new(),
            job_tx,
            tasks,
        };
        (prover, job_rx)
    }

    /// Connects the backend, with a retry until it is reachable, so a prover that is down does
    /// not stop the proxy.
    pub(crate) async fn connect(&self) {
        let backend = loop {
            match self.try_connect().await {
                Ok(backend) => break backend,
                Err(error) => {
                    warn!(proof_type = %self.proof_type, error = %format!("{error:#}"), "waiting for the prover");
                    sleep(RETRY_DELAY).await;
                }
            }
        };
        assert!(self.backend.set(backend).is_ok(), "connect is called once");
        self.metrics.connected();
        info!(proof_type = %self.proof_type, "prover ready");
    }

    async fn try_connect(&self) -> anyhow::Result<Backend> {
        Ok(match &self.config {
            BackendConfig::Ere { endpoint } => {
                let http = reqwest::Client::new();
                let client =
                    zkVMClient::new(endpoint.clone(), http, vec![Box::new(OtelPropagation)])?;
                ensure!(client.is_healthy().await, "ere-server is not healthy");
                Backend::Ere(client)
            }
            BackendConfig::Mock { .. } => Backend::Mock,
            BackendConfig::Cluster {
                endpoint,
                elf_path,
                elf_url,
            } => {
                let load = || load_elf(elf_path, elf_url);
                let elf = self.elf.get_or_try_init(load).await?.clone();
                match self.proof_type.zkvm_kind() {
                    zkVMKind::Zisk => Backend::ZiskCluster(ZiskCluster::new(endpoint, elf).await?),
                    zkVMKind::OpenVM => {
                        Backend::OpenvmCluster(Arc::new(OpenvmCluster::new(endpoint, &elf).await?))
                    }
                    _ => unreachable!("the config allows zisk and openvm clusters only"),
                }
            }
        })
    }

    pub(crate) fn connected(&self) -> bool {
        self.backend.get().is_some()
    }

    /// Puts a proof into the queue of the prover.
    pub(crate) fn enqueue(&self, job: Job) {
        // A prover that stopped does not read its queue.
        drop(self.job_tx.send(job));
    }

    /// Runs the queue. Only this task holds the queue, so no lock guards it. One proof has the
    /// turn at a time, oldest first.
    pub(crate) async fn run_queue(&self, mut job_rx: UnboundedReceiver<Job>) {
        let (idle_tx, mut idle_rx) = mpsc::unbounded_channel();
        let (mut waiting, mut proving) = (VecDeque::new(), false);
        loop {
            tokio::select! {
                // The end of a turn comes first, so a proof that arrives with it finds a place.
                biased;
                Some(()) = idle_rx.recv() => proving = false,
                Some(job) = job_rx.recv() => waiting.push_back(job),
            }
            if !proving && let Some(job) = waiting.pop_front() {
                proving = true;
                job(Turn(idle_tx.clone()));
            }
            if waiting.len() > QUEUE_CAPACITY {
                waiting.pop_front();
                error!(proof_type = %self.proof_type, "proof given up for newer payloads");
                self.metrics.dispatch_failed(DispatchFailure::QueueFull);
            }
            self.metrics.set_queue_depth(waiting.len());
        }
    }

    /// Generates the proof of a payload in its turn. The caller bounds the time, and a dropped
    /// call cancels its job on a cluster. The turn ends with the call, or after that cancel, so
    /// the next proof never meets the job before it on the cluster. A cluster records the id of
    /// the job on the current span.
    pub(crate) async fn prove(
        &self,
        turn: Turn,
        input: &ProofInput,
        gas_used: u64,
    ) -> anyhow::Result<Bytes> {
        let proof: Bytes = match self.backend.get().context("prover not connected")? {
            Backend::Ere(client) => {
                let ere_input = Input::new().with_stdin(input.stdin.clone());
                client.prove(ere_input).await?.1.0.into()
            }
            Backend::Mock => self.mock_prove(input, gas_used).await?,
            Backend::ZiskCluster(cluster) => cluster.prove(&input.stdin, &self.tasks, turn).await?,
            Backend::OpenvmCluster(cluster) => {
                cluster.prove(&input.stdin, &self.tasks, turn).await?
            }
        };
        // The beacon node takes no larger proof, so it is not kept for a submission.
        let size = proof.len();
        ensure!(
            size <= MAX_PROOF_SIZE,
            "proof of {size} bytes exceeds MAX_PROOF_SIZE"
        );
        Ok(proof)
    }

    /// Returns a `MockProof` with the public values of a valid payload and random proof bytes,
    /// after the simulated proving time.
    async fn mock_prove(&self, input: &ProofInput, gas_used: u64) -> anyhow::Result<Bytes> {
        let BackendConfig::Mock {
            mock_proving_time,
            mock_proof_size,
            mock_failure,
        } = &self.config
        else {
            unreachable!("the mock backend has the mock config")
        };
        let proving_time = match *mock_proving_time {
            MockProvingTime::Constant { ms } => ms,
            MockProvingTime::Random { min_ms, max_ms } => rng().random_range(min_ms..=max_ms),
            MockProvingTime::Linear { ms_per_mgas } => {
                ms_per_mgas.saturating_mul(gas_used).div_ceil(1_000_000)
            }
        };
        sleep(Duration::from_millis(proving_time)).await;
        ensure!(!mock_failure, "mock failure");

        let public_values = input.public_values.clone();
        // The proof data holds two list offsets and the public values beside the proof.
        let mut proof = vec![0; mock_proof_size - 8 - public_values.len()];
        rng().fill(&mut proof[..]);
        let mock_proof = MockProof {
            public_values,
            proof,
        };
        Ok(mock_proof.to_ssz().into())
    }
}

async fn load_elf(path: &Option<PathBuf>, url: &Option<String>) -> anyhow::Result<Vec<u8>> {
    match (path, url) {
        (Some(path), _) => fs::read(path)
            .await
            .with_context(|| format!("read {}", path.display())),
        (_, Some(url)) => {
            let request = reqwest::Client::new().get(url).timeout(ELF_FETCH_TIMEOUT);
            let response = request.send().await?.error_for_status()?;
            Ok(response.bytes().await?.to_vec())
        }
        (None, None) => unreachable!("the config requires a path or a URL"),
    }
}

/// Cancels a remote job in the background when the proof generation ends before the job
/// settles, for example at a timeout, so the job does not keep the cluster busy.
pub(crate) struct CancelOnDrop<F: Future<Output = anyhow::Result<()>> + Send + 'static> {
    tasks: TaskTracker,
    cancel: Option<F>,
    /// The turn of the job, which ends after the cancel.
    turn: Option<Turn>,
}

impl<F: Future<Output = anyhow::Result<()>> + Send + 'static> CancelOnDrop<F> {
    pub(crate) fn new(tasks: &TaskTracker, turn: Turn, cancel: F) -> Self {
        Self {
            tasks: tasks.clone(),
            cancel: Some(cancel),
            turn: Some(turn),
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.cancel = None;
    }
}

impl<F: Future<Output = anyhow::Result<()>> + Send + 'static> Drop for CancelOnDrop<F> {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let turn = self.turn.take();
            self.tasks.spawn(async move {
                match timeout(CANCEL_TIMEOUT, cancel).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => warn!(%error, "cluster job not cancelled"),
                    Err(_) => warn!("cluster job cancel timed out"),
                }
                drop(turn);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU64, sync::Arc};

    use tokio::{sync::mpsc, task::yield_now};
    use tokio_util::task::TaskTracker;

    use crate::{
        config::{Backend, ZkvmConfig},
        proof::{
            proof_type::ProofType,
            prover::{Prover, QUEUE_CAPACITY},
        },
    };

    /// One proof has the turn at a time, in the order of arrival. A proof that arrives at a full
    /// queue takes the place of the oldest proof that waits. A proof that arrives together with
    /// the end of a turn finds the place that the turn leaves.
    #[tokio::test]
    async fn test_queue_order_and_overflow() {
        let endpoint = "http://127.0.0.1:1".parse().unwrap();
        let config = ZkvmConfig {
            proof_type: ProofType::RethZisk,
            proof_timeout_secs: NonZeroU64::new(12).unwrap(),
            backend: Backend::Ere { endpoint },
        };
        let (prover, job_rx) = Prover::new(&config, TaskTracker::new());
        let prover = Arc::new(prover);
        tokio::spawn({
            let prover = prover.clone();
            async move { prover.run_queue(job_rx).await }
        });
        let (turn_tx, mut turn_rx) = mpsc::unbounded_channel();
        let enqueue = |arrival: usize| {
            let turn_tx = turn_tx.clone();
            prover.enqueue(Box::new(move |turn| turn_tx.send((arrival, turn)).unwrap()));
        };
        (0..=QUEUE_CAPACITY + 1).for_each(&enqueue);
        // Proof 0 has the turn, and proof 1 is the oldest one that waits.
        let expected_arrivals = [0].into_iter().chain(2..).take(4 * QUEUE_CAPACITY);
        for (arrival, next) in expected_arrivals.zip(QUEUE_CAPACITY + 2..) {
            yield_now().await;
            let (turn_arrival, turn) = turn_rx.try_recv().unwrap();
            assert_eq!(turn_arrival, arrival);
            yield_now().await;
            assert!(
                turn_rx.try_recv().is_err(),
                "the turn of {arrival} did not end"
            );
            drop(turn);
            enqueue(next);
        }
    }
}

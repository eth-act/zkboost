//! The dispatch of the proofs. It runs in the background after the response to the CL. For a
//! `VALID` payload, the dispatcher claims the proof of each prover, builds the input once, and
//! starts one task for each claimed proof. That task waits for the prover, proves, and gives
//! the proof to the submitter. The timing of every step is a local of that task.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};

use bytes::Bytes;
use tokio::{
    task::{JoinHandle, spawn_blocking},
    time::{sleep, timeout},
};
use tokio_util::{
    sync::CancellationToken,
    task::{AbortOnDropHandle, TaskTracker},
};
use tracing::{Instrument, Span, debug, error, field::Empty, info, info_span, warn};

use crate::{
    beacon::{submitter::ProofSubmitter, validator::read_keypair},
    config::Config,
    engine::payload::{NewPayloadResult, PayloadHeader, PayloadStatus, ProofInput, Receipt},
    observability::{
        dashboard::Dashboard,
        metrics::{DispatchFailure, DispatcherMetrics, ProofSteps, ProveStatus},
    },
    proof::{
        proof_cache::{Claim, ClaimOutcome, ProofCache},
        prover::{Prover, Turn},
    },
    util::RETRY_DELAY,
};

/// The request for the proofs of one payload, with the timing of its first steps.
#[derive(Clone, Copy)]
struct ProofRequest {
    header: PayloadHeader,
    received: Instant,
    /// The time from the start of the slot until the receipt.
    payload_arrival: Duration,
    witness_fetch: Duration,
}

pub(crate) struct ProofDispatcher {
    provers: Vec<Arc<Prover>>,
    cache: Arc<ProofCache>,
    submitter: Arc<ProofSubmitter>,
    metrics: DispatcherMetrics,
    latest_payload_timestamp: Mutex<u64>,
    pub(crate) dashboard: Option<Arc<Dashboard>>,
    tasks: TaskTracker,
    shutdown: CancellationToken,
}

impl ProofDispatcher {
    pub(crate) fn new(config: &Config) -> Self {
        let (tasks, shutdown) = (TaskTracker::new(), CancellationToken::new());
        let proof_types = Vec::from_iter(config.zkvm.iter().map(|zkvm| zkvm.proof_type));
        // Each prover runs its queue in a task of its own.
        let provers = config.zkvm.iter().map(|zkvm| {
            let (prover, job_rx) = Prover::new(zkvm, tasks.clone());
            let prover = Arc::new(prover);
            tasks.spawn({
                let (prover, shutdown) = (prover.clone(), shutdown.clone());
                async move {
                    shutdown.run_until_cancelled(prover.run_queue(job_rx)).await;
                }
            });
            prover
        });
        Self {
            provers: Vec::from_iter(provers),
            cache: ProofCache::new(proof_types.len()),
            submitter: Arc::new(ProofSubmitter::new(config.cl_beacon_endpoint.clone())),
            metrics: DispatcherMetrics::new(),
            latest_payload_timestamp: Mutex::new(0),
            dashboard: (config.dashboard.enabled)
                .then(|| Arc::new(Dashboard::new(proof_types, config.dashboard.retention))),
            tasks,
            shutdown,
        }
    }

    /// Runs a task in the background until it ends or the dispatcher shuts down.
    pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let shutdown = self.shutdown.clone();
        self.tasks.spawn(async move {
            shutdown.run_until_cancelled(task).await;
        });
    }

    /// Stops every task. A proof in progress on a cluster is cancelled there.
    pub(crate) async fn shutdown(&self) {
        self.shutdown.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }

    /// Reads the validator keystore, then connects the provers and runs the submitter in the
    /// background. It does nothing without a zkVM. The submitter runs in a task of its own and
    /// starts again after a panic, so a panic does not stop the proxy.
    pub(crate) async fn start(&self, config: &Config) -> anyhow::Result<()> {
        if self.provers.is_empty() {
            return Ok(());
        }
        let keystore_path = config.validator_keystore_path.clone();
        let password_path = config.validator_keystore_password_path.clone();
        let keypair =
            spawn_blocking(move || read_keypair(&keystore_path, &password_path)).await??;
        info!(pubkey = %keypair.pk, "validator keystore decrypted");
        for prover in &self.provers {
            let prover = prover.clone();
            self.spawn(async move { prover.connect().await });
        }
        let submitter = self.submitter.clone();
        self.spawn(async move {
            loop {
                let run = tokio::spawn(submitter.clone().run(keypair.clone()));
                if let Err(error) = AbortOnDropHandle::new(run).await {
                    error!(%error, "submitter stopped");
                }
                sleep(RETRY_DELAY).await;
            }
        });
        Ok(())
    }

    fn connected_provers(&self) -> impl Iterator<Item = &Arc<Prover>> {
        self.provers.iter().filter(|prover| prover.connected())
    }

    /// Returns whether a new payload is proven, which needs the validator and a prover.
    pub(crate) fn ready(&self) -> bool {
        self.submitter.validator().is_some() && self.connected_provers().next().is_some()
    }

    /// Records a new payload that the EL responded to, and dispatches the proofs of a `VALID` one.
    /// The `request_proof` span is a child of the request span, which ended with the response.
    pub(crate) async fn on_new_payload_response(
        self: Arc<Self>,
        header_task: JoinHandle<anyhow::Result<(PayloadHeader, Bytes)>>,
        result: NewPayloadResult,
        receipt: Receipt,
        request_span: Span,
    ) {
        let witness_size = result.witness_size();
        let NewPayloadResult {
            status,
            witness,
            witness_fetch,
        } = result;
        self.metrics.new_payload(status);
        let valid = status == PayloadStatus::Valid;
        let (header, params) = match header_task.await {
            Ok(Ok(decoded)) => decoded,
            error => {
                warn!(parent: &request_span, ?error, "new payload request not decodable");
                if valid && witness_fetch.is_some() {
                    self.connected_provers().for_each(|prover| {
                        prover.metrics.dispatch_failed(DispatchFailure::InputError)
                    });
                }
                return;
            }
        };
        let (block_hash, block_number) = (header.block_hash, header.block_number);
        info!(parent: &request_span, %block_hash, block_number, status = status.as_str(), "received new payload");

        let payload_arrival = (receipt.wall_clock.duration_since(UNIX_EPOCH))
            .expect("the clock is after the Unix epoch")
            .saturating_sub(Duration::from_secs(header.timestamp));
        // The EL returns a payload status only to a caller with the JWT, so only the CL moves
        // these series.
        if status != PayloadStatus::Error {
            self.metrics.payload_arrived(payload_arrival);
        }
        if valid {
            // The lock keeps the gauge from a step back when two payloads race.
            let mut latest = self.latest_payload_timestamp.lock().unwrap();
            if *latest < header.timestamp {
                *latest = header.timestamp;
                self.metrics.set_latest_payload_timestamp(header.timestamp);
            }
        }
        let Some(witness_fetch) = witness_fetch else {
            return;
        };
        if let Some(dashboard) = &self.dashboard {
            let success = witness.is_some();
            dashboard.witness_fetched(
                &header,
                receipt.wall_clock,
                witness_fetch,
                witness_size,
                success,
            );
        }
        if valid {
            let request = ProofRequest {
                header,
                received: receipt.monotonic,
                payload_arrival,
                witness_fetch,
            };
            let span = info_span!(
                parent: &request_span,
                "request_proof",
                %block_hash,
                block_number,
                timestamp = header.timestamp,
                gas_used = header.gas_used,
            );
            self.dispatch(request, params, witness)
                .instrument(span)
                .await;
        }
    }

    /// Claims the proof of each prover, submits the proven ones again, and starts a task for
    /// each of the others. A request without a witness, which the EL sends for a payload that
    /// it knows, reads the proven ones and claims nothing.
    async fn dispatch(
        self: &Arc<Self>,
        request: ProofRequest,
        params: Bytes,
        witness: Option<Bytes>,
    ) {
        let header = request.header;
        let mut claims = Vec::new();
        for prover in self.connected_provers() {
            let proof_type = prover.proof_type;
            match (self.cache).claim(header.block_hash, proof_type, witness.is_some()) {
                ClaimOutcome::Proven(proof) => {
                    info!(%proof_type, "proof reused");
                    prover.metrics.reused();
                    let (submitter, prover) = (self.submitter.clone(), prover.clone());
                    self.spawn(
                        async move {
                            submitter.submit(&prover, header, proof).await;
                        }
                        .in_current_span(),
                    );
                }
                ClaimOutcome::Claimed => debug!(%proof_type, "proof already requested"),
                ClaimOutcome::New(claim) => claims.push((prover.clone(), claim)),
                ClaimOutcome::NoWitness => {
                    error!(%proof_type, "valid payload without witness");
                    (prover.metrics).dispatch_failed(DispatchFailure::MissingWitness);
                }
            }
        }
        let Some(witness) = witness.filter(|_| !claims.is_empty()) else {
            return;
        };

        let input = match self.build_input(header.slot, params, witness).await {
            Ok(input) => Arc::new(input),
            Err(reason) => {
                (claims.iter()).for_each(|(prover, _)| prover.metrics.dispatch_failed(reason));
                return;
            }
        };
        let queued = Instant::now();
        for (prover, claim) in claims {
            let (dispatcher, input, span) = (self.clone(), input.clone(), Span::current());
            // The proof starts when the prover gives it the turn.
            prover.clone().enqueue(Box::new(move |turn| {
                let proof_task =
                    (dispatcher.clone()).prove(turn, prover, claim, request, input, queued);
                dispatcher.spawn(proof_task.instrument(span));
            }));
        }
    }

    /// Builds the input of the provers on the blocking pool. A failure is logged here, and
    /// its reason goes to the caller.
    async fn build_input(
        &self,
        slot: u64,
        params: Bytes,
        witness: Bytes,
    ) -> Result<ProofInput, DispatchFailure> {
        let validator = (self.submitter.validator()).expect("a proven payload has a validator");
        let fork = validator.protocol_fork(slot).map_err(|error| {
            error!(%error, "payload of an unsupported fork");
            DispatchFailure::UnsupportedFork
        })?;
        let chain_id = validator.chain_id();
        let input = spawn_blocking(move || ProofInput::build(&params, &witness, fork, chain_id));
        match input.await {
            Ok(Ok(input)) => Ok(input),
            error => {
                error!(?error, "proof input not built");
                Err(DispatchFailure::InputError)
            }
        }
    }

    /// Generates one proof in its turn and gives it to the submitter. `queued` is the time when
    /// the proof went into the queue.
    async fn prove(
        self: Arc<Self>,
        turn: Turn,
        prover: Arc<Prover>,
        claim: Claim,
        request: ProofRequest,
        input: Arc<ProofInput>,
        queued: Instant,
    ) {
        let (header, proof_type) = (request.header, prover.proof_type);
        let queue_wait = queued.elapsed();
        prover.metrics.queue_waited(queue_wait);
        if let Some(dashboard) = &self.dashboard {
            dashboard.prove_started(header.block_hash, proof_type);
        }

        let span = info_span!(
            "prove",
            otel.name = format!("prove/{proof_type}"),
            %proof_type,
            job_id = Empty,
            otel.status_code = Empty,
            error_reason = Empty,
        );
        info!(parent: &span, "proving");
        let started = Instant::now();
        let result = timeout(prover.timeout, prover.prove(turn, &input, header.gas_used))
            .instrument(span.clone())
            .await;
        let prove = started.elapsed();

        let (status, error) = match &result {
            Ok(Ok(_)) => (ProveStatus::Success, None),
            Ok(Err(error)) => (ProveStatus::Error, Some(format!("{error:#}"))),
            Err(_) => (ProveStatus::Timeout, None),
        };
        let proof = result.ok().and_then(Result::ok);
        let proof_size = proof.as_ref().map(Bytes::len);
        prover
            .metrics
            .proved(status, prove, proof_size.unwrap_or(0));
        if let Some(dashboard) = &self.dashboard {
            dashboard.prove_ended(
                header.block_hash,
                proof_type,
                status,
                error.clone(),
                proof_size,
            );
        }
        let Some(proof) = proof else {
            let reason = error.as_deref().unwrap_or("timeout");
            span.record("otel.status_code", "ERROR");
            span.record("error_reason", reason);
            error!(parent: &span, reason, "proving failed");
            return;
        };
        info!(parent: &span, proof_size, "proved");

        claim.complete(proof.clone());
        if let Some(submission) = self.submitter.submit(&prover, header, proof).await {
            let steps = ProofSteps {
                payload_arrival: request.payload_arrival,
                witness_fetch: request.witness_fetch,
                queue_wait,
                prove,
                submission,
            };
            prover.metrics.accepted(request.received, &steps);
        }
    }
}

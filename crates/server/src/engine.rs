//! Engine API proxy state. Every request is forwarded to the EL with the caller's `Authorization`
//! header unchanged. An `engine_newPayload` of a proven version goes upstream as
//! `engine_newPayloadWithWitness` of the same version, and the witness is removed from the status.
//! A `VALID` payload is proven and submitted. Without a zkVM, every request is forwarded unchanged.
//! The witness is the RLP list `[headers, codes, state]` and an optional, ignored `keys` list.

use std::{
    collections::{HashMap, hash_map::Entry},
    fs,
    num::NonZeroUsize,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

use alloy_primitives::B256;
use alloy_rlp::{Header, PayloadView};
use anyhow::{Context, anyhow, bail};
use axum::http::HeaderValue;
use beacon_node_client::{BEACON_NODE_RETRY_DELAY, BeaconNodeClient, ExecutionPayloadEvent};
use bytes::Bytes;
use engine_api::{
    EngineApiClient, EngineResponse, JsonRpcRequest, JsonRpcResponse, NewPayloadStatus,
    NewPayloadWithWitnessResponse,
};
use lighthouse_bls::Keypair;
use lighthouse_eth2_keystore::Keystore;
use lighthouse_types::{ChainSpec, Slot};
use lru::LruCache;
use tokio::{
    sync::{mpsc, mpsc::error::TrySendError},
    task::JoinHandle,
};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, debug, error, info, info_span, warn};
use validator::Validator;
use zkboost_types::{
    ExecutionWitness, Hash256, NewPayloadRequest, NewPayloadRequestExt, NewPayloadVersion,
    ProgressiveList, ProofType, SszList,
};

use crate::{
    config::Config,
    dashboard::DashboardMessage,
    metrics::{
        PayloadTiming, ProofTiming, add_queue_depth, duration_since_timestamp, record_new_payload,
        record_payload_arrival, record_proof_dispatch_failure, record_proof_latency,
        record_proof_reused, record_submission, record_witness_fetch, set_latest_payload_timestamp,
        set_validator_ready,
    },
    proof::{
        input::{NewPayloadRequestMeta, StatelessInput},
        worker::{ProofResult, WorkerInput, WorkerOutput},
    },
};

pub(crate) mod beacon_node_client;
pub(crate) mod engine_api;
pub(crate) mod validator;

/// JSON-RPC error code of a method the EL does not serve.
const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;
/// Proofs kept per proof type. Two epochs cover a payload imported again after a reorg.
const PROOF_CACHE_SLOTS: usize = 64;
/// Blocks kept from the `execution_payload` events, two epochs as the proof cache.
const BLOCK_CACHE_SLOTS: usize = 64;
/// Attempts to submit a proof, `BEACON_NODE_RETRY_DELAY` apart, which span less than the 12 second
/// slot.
const SUBMISSION_ATTEMPTS: u32 = 5;

/// The receipt of a payload, read once on each clock. The wall clock places it in its slot, and
/// every later time is measured on the monotonic clock from it.
#[derive(Debug, Clone, Copy)]
struct PayloadReceipt {
    time: SystemTime,
    instant: Instant,
}

/// The outcome of the forward of a new payload to the EL, for the work that follows the answer.
#[derive(Debug)]
struct NewPayloadOutcome {
    /// The payload status, `None` without a result or with an unknown status.
    status: Option<NewPayloadStatus>,
    /// The witness of a `VALID` payload.
    witness: Option<Bytes>,
    /// The time from the receipt until the EL returned the witness, `None` without a witness
    /// fetch.
    duration: Option<Duration>,
}

/// The state of a proof of a payload before a claim.
#[derive(Debug)]
enum ProofState {
    /// Proven, with the proof.
    Proven(Vec<u8>),
    /// Claimed by an earlier request.
    InProgress,
    /// Neither proven nor in progress, so the claim takes it.
    Unproven,
}

/// The proven proofs, kept for a payload imported again, and the proofs in progress. One lock
/// guards both, so every proof is claimed once.
#[derive(Debug)]
struct Proofs {
    proven: LruCache<(Hash256, ProofType), Vec<u8>>,
    in_progress: HashMap<(Hash256, ProofType), PayloadTiming>,
}

impl Proofs {
    fn new(capacity: NonZeroUsize) -> Self {
        Self {
            proven: LruCache::new(capacity),
            in_progress: HashMap::new(),
        }
    }

    /// Claims a proof of a payload that is neither proven nor in progress, and returns its state
    /// before the claim.
    fn claim(&mut self, root: Hash256, proof_type: ProofType, timing: PayloadTiming) -> ProofState {
        if let Some(proof) = self.proven.get(&(root, proof_type)) {
            return ProofState::Proven(proof.clone());
        }
        match self.in_progress.entry((root, proof_type)) {
            Entry::Occupied(_) => ProofState::InProgress,
            Entry::Vacant(entry) => {
                entry.insert(timing);
                ProofState::Unproven
            }
        }
    }

    /// Ends the claim of a proof, keeps the proof of a successful one, and returns the timing of
    /// its payload.
    fn end_claim(
        &mut self,
        root: Hash256,
        proof_type: ProofType,
        proof: Option<Vec<u8>>,
    ) -> Option<PayloadTiming> {
        if let Some(proof) = proof {
            self.proven.put((root, proof_type), proof);
        }
        self.in_progress.remove(&(root, proof_type))
    }
}

/// Engine API proxy with the proving pipeline of every valid payload.
#[allow(missing_debug_implementations)]
pub(crate) struct EngineProxyState {
    /// Client of the EL that every request is forwarded to.
    pub(crate) engine_api_client: EngineApiClient,
    beacon_node_client: BeaconNodeClient,
    keypair: Keypair,
    validator: OnceLock<Validator>,
    /// Root of the beacon block that carries a payload, by the block hash and the slot of the
    /// payload.
    blocks: Mutex<LruCache<(B256, Slot), B256>>,
    proofs: Mutex<Proofs>,
    /// Timestamp of the newest VALID payload, the start of its slot.
    latest_payload_timestamp: Mutex<u64>,
    worker_input_txs: HashMap<ProofType, mpsc::Sender<WorkerInput>>,
    dashboard_service_tx: mpsc::Sender<DashboardMessage>,
}

impl EngineProxyState {
    /// Creates the proxy of the configured EL with the input channel of every zkVM worker. It
    /// decrypts the validator keystore.
    pub(crate) fn new(
        config: &Config,
        worker_input_txs: HashMap<ProofType, mpsc::Sender<WorkerInput>>,
        dashboard_service_tx: mpsc::Sender<DashboardMessage>,
    ) -> anyhow::Result<Self> {
        let keypair = read_keypair(
            &config.validator_keystore_path,
            &config.validator_keystore_password_path,
        )?;
        info!(pubkey = %keypair.pk, "validator keystore decrypted");
        let proofs_capacity = NonZeroUsize::new(PROOF_CACHE_SLOTS * worker_input_txs.len())
            .unwrap_or(NonZeroUsize::MIN);
        Ok(Self {
            engine_api_client: EngineApiClient::new(config.el_engine_endpoint.clone()),
            beacon_node_client: BeaconNodeClient::new(config.cl_beacon_endpoint.clone())?,
            keypair,
            validator: OnceLock::new(),
            blocks: Mutex::new(LruCache::new(NonZeroUsize::new(BLOCK_CACHE_SLOTS).unwrap())),
            proofs: Mutex::new(Proofs::new(proofs_capacity)),
            latest_payload_timestamp: Mutex::new(0),
            worker_input_txs,
            dashboard_service_tx,
        })
    }

    /// Waits until the beacon node answers the startup reads. It then keeps the block cache from
    /// the events and records every proof attempt of the workers until the shutdown. Without a
    /// zkVM, nothing is proven, so it returns at once.
    pub(crate) async fn run(
        self: Arc<Self>,
        shutdown: CancellationToken,
        mut worker_output_rx: mpsc::Receiver<WorkerOutput>,
    ) {
        if self.worker_input_txs.is_empty() {
            return;
        }
        set_validator_ready(false);

        let (spec, genesis_validators_root, validator_index) = tokio::select! {
            () = shutdown.cancelled() => return,
            values = async {
                loop {
                    match self.startup_reads().await {
                        Ok(values) => break values,
                        Err(error) => {
                            warn!(error = %format!("{error:#}"), "waiting for the beacon node");
                            tokio::time::sleep(BEACON_NODE_RETRY_DELAY).await;
                        }
                    }
                }
            } => values,
        };

        let validator = Validator::new(
            self.keypair.clone(),
            spec,
            genesis_validators_root,
            validator_index,
        );
        assert!(self.validator.set(validator).is_ok(), "run is called once");
        set_validator_ready(true);
        info!(validator_index, "validator ready");

        let mut events = self.beacon_node_client.subscribe_execution_payloads();
        loop {
            tokio::select! {
                biased;

                () = shutdown.cancelled() => break,

                event = events.next() => match event {
                    Some(event) => self.insert_block(&event),
                    None => break,
                },

                output = worker_output_rx.recv() => match output {
                    Some(output) => self.complete_proof(output),
                    None => break,
                },
            }
        }
    }

    /// Reads the chain spec, the genesis validators root, and the index of the validator.
    async fn startup_reads(&self) -> anyhow::Result<(ChainSpec, B256, u64)> {
        let spec = self.beacon_node_client.spec().await?;
        let genesis_validators_root = self.beacon_node_client.genesis_validators_root().await?;
        let validator_index = self
            .beacon_node_client
            .validator_index(&self.keypair.pk)
            .await?;
        Ok((spec, genesis_validators_root, validator_index))
    }

    /// Inserts the beacon block root that an `execution_payload` event reports for a payload.
    /// The first root of a payload is kept.
    fn insert_block(&self, event: &ExecutionPayloadEvent) {
        let ExecutionPayloadEvent {
            slot,
            block_hash,
            block_root,
        } = *event;
        let mut blocks = self.blocks.lock().unwrap();
        if *blocks.get_or_insert((block_hash, slot), || block_root) != block_root {
            warn!(%block_hash, %slot, "second beacon block for the payload ignored");
        }
    }

    /// Forwards a new payload, with the witness method when it can prove, and answers without the
    /// witness. The work that needs the payload continues in the background.
    pub(crate) async fn new_payload(
        self: Arc<Self>,
        authorization: Option<&HeaderValue>,
        body: Bytes,
        request: JsonRpcRequest,
        version: NewPayloadVersion,
    ) -> reqwest::Result<EngineResponse> {
        let receipt = PayloadReceipt {
            time: SystemTime::now(),
            instant: Instant::now(),
        };
        let proving = !self.worker_input_txs.is_empty() && self.validator().is_ok();
        let request = request.with_method(version.with_witness_method());
        let body_with_witness = proving.then(|| Bytes::from(serde_json::to_vec(&request).unwrap()));
        let decoding = tokio::task::spawn_blocking(move || {
            let payload = version.decode_params(request.params.get())?;
            anyhow::Ok((NewPayloadRequestMeta::new(&payload)?, payload))
        });

        let (response, outcome) = match body_with_witness {
            Some(body_with_witness) => {
                self.forward_with_witness(authorization, body, body_with_witness, receipt.instant)
                    .await
            }
            None => self.forward_without_witness(authorization, body).await,
        };
        tokio::spawn(self.complete_new_payload(decoding, outcome, receipt, Span::current()));
        response
    }

    /// Forwards the payload as `engine_newPayload` without a witness fetch.
    async fn forward_without_witness(
        &self,
        authorization: Option<&HeaderValue>,
        body: Bytes,
    ) -> (reqwest::Result<EngineResponse>, NewPayloadOutcome) {
        let response = self.engine_api_client.forward(authorization, body).await;
        let status = response
            .as_ref()
            .ok()
            .and_then(|response| new_payload_status(&response.body));
        let outcome = NewPayloadOutcome {
            status,
            witness: None,
            duration: None,
        };
        (response, outcome)
    }

    /// Forwards the payload as `engine_newPayloadWithWitness`, or as `engine_newPayload` if the EL
    /// does not serve that method, and records the witness fetch.
    async fn forward_with_witness(
        &self,
        authorization: Option<&HeaderValue>,
        body: Bytes,
        body_with_witness: Bytes,
        received_at: Instant,
    ) -> (reqwest::Result<EngineResponse>, NewPayloadOutcome) {
        let response = self
            .engine_api_client
            .forward(authorization, body_with_witness)
            .await;
        let duration = received_at.elapsed();
        let fetched = |fetch_result, status, witness: Option<Bytes>| {
            record_witness_fetch(
                fetch_result,
                duration,
                witness.as_ref().map_or(0, Bytes::len),
            );
            NewPayloadOutcome {
                status,
                witness,
                duration: Some(duration),
            }
        };

        let response = match response {
            Ok(response) => response,
            Err(error) => return (Err(error), fetched("error", None, None)),
        };
        let Ok(mut response_body) = serde_json::from_slice::<
            JsonRpcResponse<NewPayloadWithWitnessResponse>,
        >(&response.body) else {
            return (Ok(response), fetched("error", None, None));
        };
        if response_body
            .error
            .as_ref()
            .is_some_and(|error| error.code == JSON_RPC_METHOD_NOT_FOUND)
        {
            warn!("witness method not served by the el, forwarded without proving");
            record_witness_fetch("missing", duration, 0);
            return self.forward_without_witness(authorization, body).await;
        }

        let status = response_body.status();
        let witness = response_body
            .result
            .as_mut()
            .and_then(|result| result.witness.take());
        // Only a VALID status comes with a witness.
        let (fetch_result, witness) = match (status, witness) {
            (Some(NewPayloadStatus::Valid), Some(witness)) => ("success", Some(witness.0)),
            (Some(NewPayloadStatus::Valid), None) => ("missing", None),
            (Some(_), _) => ("not_valid", None),
            (None, _) => ("error", None),
        };
        let response = EngineResponse {
            status: response.status,
            body: serde_json::to_vec(&response_body).unwrap().into(),
        };
        (Ok(response), fetched(fetch_result, status, witness))
    }

    /// Records a new payload on the metrics and the dashboard, and requests the proofs of a
    /// `VALID` payload. The `request_proof` span has the request span as parent, which this task
    /// never enters, so the request span ends with the answer.
    async fn complete_new_payload(
        self: Arc<Self>,
        decoding: JoinHandle<anyhow::Result<(NewPayloadRequestMeta, NewPayloadRequest)>>,
        outcome: NewPayloadOutcome,
        receipt: PayloadReceipt,
        parent: Span,
    ) {
        let NewPayloadOutcome {
            status,
            witness,
            duration,
        } = outcome;
        let status_label = status.map_or("error", NewPayloadStatus::as_str);
        record_new_payload(status_label);
        let valid = status == Some(NewPayloadStatus::Valid);
        let decoded = decoding.await.unwrap_or_else(|error| Err(error.into()));
        let (payload_meta, payload) = match decoded {
            Ok(payload) => payload,
            Err(error) => {
                warn!(%error, "new payload params not decodable, skip proving");
                if duration.is_some() && valid {
                    for &proof_type in self.worker_input_txs.keys() {
                        record_proof_dispatch_failure(proof_type, "input_error");
                    }
                }
                return;
            }
        };
        let block_hash = payload_meta.block_hash;
        let block_number = payload_meta.block_number;
        info!(%block_hash, block_number, "received new payload");
        let arrival = duration_since_timestamp(payload_meta.timestamp, receipt.time);
        // The EL answers a payload status only with the JWT, and checks the timestamp of a VALID
        // payload, so a caller without the JWT moves neither series.
        if status.is_some() {
            record_payload_arrival(arrival);
        }
        if valid {
            let mut latest_payload_timestamp = self.latest_payload_timestamp.lock().unwrap();
            if payload_meta.timestamp > *latest_payload_timestamp {
                *latest_payload_timestamp = payload_meta.timestamp;
                set_latest_payload_timestamp(payload_meta.timestamp);
            }
        }

        let Some(witness_fetch) = duration else {
            debug!(%block_hash, "witness not fetched, skip proving");
            return;
        };
        let timing = PayloadTiming {
            arrival,
            received_at: receipt.instant,
            witness_fetch,
        };
        self.notify_dashboard(DashboardMessage::fetch_witness_start(
            block_number,
            block_hash,
            payload_meta.timestamp,
            payload_meta.gas_used,
            payload.gas_limit(),
            receipt.time,
        ));
        self.notify_dashboard(DashboardMessage::fetch_witness_end(
            block_hash,
            witness.as_ref().map_or(0, Bytes::len),
            witness.is_some(),
            receipt.time + witness_fetch,
        ));
        if !valid {
            debug!(%block_hash, status = status_label, "payload not valid, skip proving");
            return;
        }

        let span = info_span!(
            parent: &parent,
            "request_proof",
            %block_hash,
            block_number,
            timestamp = payload_meta.timestamp,
            gas_used = payload_meta.gas_used
        );
        if let Err(error) = self
            .request_proofs(payload_meta, payload, witness, timing)
            .instrument(span.clone())
            .await
        {
            error!(parent: &span, %block_hash, %error, "proof request failed");
        }
    }

    /// Claims the proofs of a payload, submits the proven ones again, and queues the stateless
    /// input of the others at their workers.
    async fn request_proofs(
        self: &Arc<Self>,
        payload_meta: NewPayloadRequestMeta,
        payload: NewPayloadRequest,
        witness: Option<Bytes>,
        timing: PayloadTiming,
    ) -> anyhow::Result<()> {
        let validator = self.validator()?;
        let root = payload_meta.new_payload_request_root;

        let mut claimed = Vec::new();
        for (&proof_type, worker_input_tx) in &self.worker_input_txs {
            let claim = self.proofs.lock().unwrap().claim(root, proof_type, timing);
            match claim {
                ProofState::Proven(proof) => {
                    info!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, "proof reused");
                    record_proof_reused(proof_type);
                    self.submit_proof(payload_meta, proof_type, proof, None);
                }
                // A proof in progress needs no input, which a payload imported again lacks.
                ProofState::InProgress => {
                    debug!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, "proof already requested");
                }
                ProofState::Unproven => claimed.push((proof_type, worker_input_tx)),
            }
        }
        if claimed.is_empty() {
            return Ok(());
        }
        let end_claims = |reason| {
            for &(proof_type, _) in &claimed {
                self.abandon_claim(root, proof_type, reason);
            }
        };

        let Some(witness) = witness else {
            end_claims("missing_witness");
            bail!("valid payload without witness");
        };
        let protocol_fork = validator
            .protocol_fork(Slot::new(payload_meta.slot))
            .inspect_err(|_| end_claims("unsupported_fork"))?;
        let chain_id = validator.chain_id();
        let stateless_input = tokio::task::spawn_blocking(move || {
            let witness = decode_engine_witness(&witness)?;
            anyhow::Ok(StatelessInput::new(
                protocol_fork,
                payload_meta,
                payload,
                witness,
                chain_id,
            ))
        })
        .await
        .expect("stateless input construction does not panic")
        .context("stateless input construction")
        .inspect_err(|_| end_claims("input_error"))?;
        let stateless_input = Arc::new(stateless_input);

        for (proof_type, worker_input_tx) in claimed {
            let worker_input = WorkerInput {
                stateless_input: stateless_input.clone(),
                span: Span::current(),
                queued_at: Instant::now(),
            };
            // The depth grows before the send, so the dequeue of the worker never takes it below 0.
            add_queue_depth(proof_type, 1.0);
            match worker_input_tx.try_send(worker_input) {
                Ok(()) => {
                    debug!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, "proof dispatched")
                }
                Err(error) => {
                    add_queue_depth(proof_type, -1.0);
                    let (reason, label) = match error {
                        TrySendError::Full(_) => ("worker channel full", "channel_full"),
                        TrySendError::Closed(_) => ("worker channel closed", "channel_closed"),
                    };
                    error!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, reason, "proof dispatch failed");
                    self.abandon_claim(root, proof_type, label);
                }
            }
        }
        Ok(())
    }

    /// Ends the claim of a proof whose request fails before its input reaches the worker.
    fn abandon_claim(&self, root: Hash256, proof_type: ProofType, reason: &'static str) {
        record_proof_dispatch_failure(proof_type, reason);
        self.proofs
            .lock()
            .unwrap()
            .end_claim(root, proof_type, None);
    }

    /// Records the outcome of a proof attempt and submits a proof to the beacon node.
    fn complete_proof(self: &Arc<Self>, output: WorkerOutput) {
        let WorkerOutput {
            payload_meta,
            proof_type,
            proof_result,
            queue_wait,
            prove_duration,
        } = output;
        let proof = match &proof_result {
            ProofResult::Ok(proof) => Some(proof.clone()),
            ProofResult::Err(_) | ProofResult::Timeout => None,
        };
        let payload_timing = self.proofs.lock().unwrap().end_claim(
            payload_meta.new_payload_request_root,
            proof_type,
            proof,
        );
        match proof_result {
            ProofResult::Ok(proof) => {
                info!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, proof_size = proof.len(), "proved");
                let timing = payload_timing.map(|payload| ProofTiming {
                    payload,
                    queue_wait,
                    prove: prove_duration,
                });
                self.submit_proof(payload_meta, proof_type, proof, timing);
            }
            ProofResult::Err(error) => {
                error!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, %error, "proving failed");
            }
            ProofResult::Timeout => {
                error!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, "proving timed out");
            }
        }
    }

    /// Submits a proof to the beacon node in the background, with `SUBMISSION_ATTEMPTS` attempts
    /// `BEACON_NODE_RETRY_DELAY` apart. A reused proof has no `timing`, so its latencies are not
    /// recorded.
    fn submit_proof(
        self: &Arc<Self>,
        payload_meta: NewPayloadRequestMeta,
        proof_type: ProofType,
        proof: Vec<u8>,
        timing: Option<ProofTiming>,
    ) {
        let state = self.clone();
        let slot = Slot::new(payload_meta.slot);
        tokio::spawn(async move {
            let started = Instant::now();
            let record = |status| record_submission(proof_type, status, started.elapsed());
            for attempt in 1..=SUBMISSION_ATTEMPTS {
                let submission = async {
                    let announced = state
                        .blocks
                        .lock()
                        .unwrap()
                        .get(&(payload_meta.block_hash, slot))
                        .copied();
                    let beacon_block_root = match announced {
                        Some(beacon_block_root) => beacon_block_root,
                        None => {
                            state
                                .beacon_node_client
                                .beacon_block_root(
                                    payload_meta.block_hash,
                                    payload_meta.parent_beacon_block_root,
                                    slot,
                                )
                                .await?
                        }
                    };
                    let envelopes = state.validator()?.sign_execution_proofs(
                        beacon_block_root,
                        slot,
                        proof_type.execution_proof_type(),
                        proof.clone(),
                    )?;
                    state
                        .beacon_node_client
                        .post_execution_proofs(&envelopes)
                        .await
                };
                match submission.await {
                    Ok(()) => {
                        info!(block_hash = %payload_meta.block_hash, %proof_type, "proof submitted");
                        record("success");
                        if let Some(timing) = timing {
                            record_proof_latency(proof_type, timing, started.elapsed());
                        }
                        break;
                    }
                    Err(error) if attempt < SUBMISSION_ATTEMPTS => {
                        warn!(block_hash = %payload_meta.block_hash, %proof_type, attempt, error = %format!("{error:#}"), "proof submission retried");
                        tokio::time::sleep(BEACON_NODE_RETRY_DELAY).await;
                    }
                    Err(error) => {
                        error!(block_hash = %payload_meta.block_hash, %proof_type, error = %format!("{error:#}"), "proof submission failed");
                        record("error");
                    }
                }
            }
        });
    }

    /// Sends a message to the dashboard service, which is absent when the dashboard is disabled.
    fn notify_dashboard(&self, message: DashboardMessage) {
        let _ = self.dashboard_service_tx.try_send(message);
    }

    /// Returns the validator, which is ready once the beacon node answers the startup reads.
    fn validator(&self) -> anyhow::Result<&Validator> {
        self.validator.get().context("validator not ready")
    }
}

/// Reads the keypair of the validator from an EIP-2335 keystore and its password file. Trailing
/// newlines are not part of the password, as in lighthouse.
fn read_keypair(keystore_path: &Path, password_path: &Path) -> anyhow::Result<Keypair> {
    let keystore = Keystore::from_json_file(keystore_path)
        .map_err(|error| anyhow!("read keystore {}: {error:?}", keystore_path.display()))?;
    let password = fs::read(password_path)
        .with_context(|| format!("read password {}", password_path.display()))?;
    let password_end = password
        .iter()
        .rposition(|byte| !matches!(byte, b'\n' | b'\r'))
        .map_or(0, |position| position + 1);
    keystore
        .decrypt_keypair(&password[..password_end])
        .map_err(|error| anyhow!("decrypt keystore: {error:?}"))
}

/// Returns the payload status of the EL answer to an `engine_newPayload`, `None` without a result
/// or with an unknown status.
fn new_payload_status(body: &[u8]) -> Option<NewPayloadStatus> {
    serde_json::from_slice::<JsonRpcResponse<NewPayloadWithWitnessResponse>>(body)
        .ok()?
        .status()
}

/// Decodes the RLP-encoded engine witness into the `ExecutionWitness`.
fn decode_engine_witness(bytes: &[u8]) -> anyhow::Result<ExecutionWitness> {
    let mut fields = rlp_list(bytes, "witness")?.into_iter();
    let mut next_field = |label: &str| {
        fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("witness is missing the {label} field"))
    };
    let headers = rlp_list(next_field("headers")?, "witness headers")?;
    let codes = rlp_byte_strings(next_field("codes")?, "witness codes")?;
    let state = rlp_byte_strings(next_field("state")?, "witness state")?;
    let headers = ssz_bytes_list(headers, "witness headers")?;
    Ok(ExecutionWitness {
        state: ProgressiveList::from(ssz_bytes_list(state, "witness state")?),
        codes: ProgressiveList::from(ssz_bytes_list(codes, "witness codes")?),
        headers: SszList::try_from(headers).map_err(|error| {
            anyhow::anyhow!("witness headers length should be within bounds: {error:?}")
        })?,
    })
}

/// Splits an RLP list into the raw encoding of each of its items.
fn rlp_list<'a>(bytes: &'a [u8], label: &str) -> anyhow::Result<Vec<&'a [u8]>> {
    match Header::decode_raw(&mut &*bytes) {
        Ok(PayloadView::List(items)) => Ok(items),
        Ok(PayloadView::String(_)) => anyhow::bail!("{label} is not an RLP list"),
        Err(error) => anyhow::bail!("{label} is not decodable: {error}"),
    }
}

/// Decodes an RLP list of byte strings.
fn rlp_byte_strings<'a>(bytes: &'a [u8], label: &str) -> anyhow::Result<Vec<&'a [u8]>> {
    rlp_list(bytes, label)?
        .into_iter()
        .map(|item| match Header::decode_raw(&mut &*item) {
            Ok(PayloadView::String(string)) => Ok(string),
            Ok(PayloadView::List(_)) => anyhow::bail!("{label} item is not an RLP string"),
            Err(error) => anyhow::bail!("{label} item is not decodable: {error}"),
        })
        .collect()
}

fn ssz_bytes_list<const M: usize>(
    items: Vec<&[u8]>,
    label: &str,
) -> anyhow::Result<Vec<SszList<u8, M>>> {
    items
        .into_iter()
        .map(|item| SszList::try_from(item.to_vec()))
        .collect::<Result<_, _>>()
        .map_err(|error| anyhow::anyhow!("{label} item length should be within bounds: {error:?}"))
}

#[cfg(test)]
mod tests {
    use std::{
        num::NonZeroUsize,
        time::{Duration, Instant},
    };

    use alloy_rlp::Header;
    use serde_json::json;
    use zkboost_types::{Hash256, ProofType};

    use crate::{
        engine::{
            ProofState, Proofs, decode_engine_witness,
            engine_api::{JsonRpcResponse, NewPayloadStatus, NewPayloadWithWitnessResponse},
            new_payload_status,
        },
        metrics::PayloadTiming,
    };

    #[test]
    fn test_new_payload_status_is_bounded() {
        let answer = |result: &str| format!(r#"{{"jsonrpc":"2.0","id":1,"result":{result}}}"#);
        for status in ["VALID", "INVALID", "SYNCING", "ACCEPTED"] {
            let body = answer(&format!(
                r#"{{"status":"{status}","latestValidHash":null}}"#
            ));
            assert_eq!(
                new_payload_status(body.as_bytes()).map(NewPayloadStatus::as_str),
                Some(status)
            );
        }
        let unknown = answer(r#"{"status":"INVALID_BLOCK_HASH"}"#);
        assert_eq!(new_payload_status(unknown.as_bytes()), None);
        let error = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"no method"}}"#;
        assert_eq!(new_payload_status(error.as_bytes()), None);
        assert_eq!(new_payload_status(b"not json"), None);
    }

    /// An answer with an unknown status passes through without the witness.
    #[test]
    fn test_witness_removed_with_unknown_status() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"status":"UNKNOWN","latestValidHash":null,"witness":"0xc0"}}"#;
        let response =
            serde_json::from_str::<JsonRpcResponse<NewPayloadWithWitnessResponse>>(body).unwrap();
        assert_eq!(response.status(), None);
        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            json!({"jsonrpc":"2.0","id":1,"result":{"status":"UNKNOWN","latestValidHash":null}})
        );
    }

    /// A proof is claimed only while it is neither proven nor in progress.
    #[test]
    fn test_proof_claimed_once() {
        let mut proofs = Proofs::new(NonZeroUsize::MIN);
        let (root, proof_type) = (Hash256::ZERO, ProofType::RethOpenVM);
        let timing = PayloadTiming {
            arrival: Duration::ZERO,
            received_at: Instant::now(),
            witness_fetch: Duration::ZERO,
        };

        assert!(matches!(
            proofs.claim(root, proof_type, timing),
            ProofState::Unproven
        ));
        assert!(matches!(
            proofs.claim(root, proof_type, timing),
            ProofState::InProgress
        ));

        assert!(proofs.end_claim(root, proof_type, None).is_some());
        assert!(matches!(
            proofs.claim(root, proof_type, timing),
            ProofState::Unproven
        ));

        assert!(proofs.end_claim(root, proof_type, Some(vec![1])).is_some());
        assert!(matches!(
            proofs.claim(root, proof_type, timing),
            ProofState::Proven(proof) if proof == [1]
        ));
        assert!(proofs.in_progress.is_empty());
    }

    /// Wraps already RLP-encoded items into an RLP list.
    fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        Header {
            list: true,
            payload_length: items.iter().map(Vec::len).sum(),
        }
        .encode(&mut out);
        items.iter().for_each(|item| out.extend(item));
        out
    }

    fn rlp_strings(items: &[&[u8]]) -> Vec<u8> {
        rlp_list(&items.iter().map(alloy_rlp::encode).collect::<Vec<_>>())
    }

    /// A header in the witness is kept as its raw RLP encoding, and the trailing `keys` list
    /// emitted by some clients is ignored.
    #[test]
    fn test_decode_engine_witness_keeps_header_encoding_and_ignores_keys() {
        let header = alloy_rlp::encode(vec![vec![0xaau8; 32], vec![0x01u8]]);
        let encoded = rlp_list(&[
            rlp_list(std::slice::from_ref(&header)),
            rlp_strings(&[&[0x60, 0x00]]),
            rlp_strings(&[&[0xf8; 3], &[]]),
            rlp_strings(&[&[0x02]]),
        ]);

        let decoded = decode_engine_witness(&encoded).unwrap();

        assert_eq!(decoded.headers.len(), 1);
        assert_eq!(decoded.headers[0].to_vec(), header);
        assert_eq!(decoded.codes.len(), 1);
        assert_eq!(decoded.codes[0].to_vec(), vec![0x60, 0x00]);
        assert_eq!(decoded.state.len(), 2);
        assert_eq!(decoded.state[0].to_vec(), vec![0xf8; 3]);
        assert!(decoded.state[1].is_empty());
    }

    #[test]
    fn test_decode_engine_witness_rejects_short_list() {
        let encoded = rlp_list(&[rlp_list(&[]), rlp_strings(&[])]);
        let error = decode_engine_witness(&encoded).unwrap_err().to_string();
        assert!(error.contains("missing the state field"), "{error}");
    }
}

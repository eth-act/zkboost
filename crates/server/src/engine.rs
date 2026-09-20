//! Engine API proxy state. Every request is forwarded to the EL with the caller's `Authorization`
//! header unchanged. `engine_newPayloadV5` goes upstream as `engine_newPayloadWithWitnessV5`, and
//! the witness is removed from the status. A `VALID` payload is proven and submitted.
//! The witness is the RLP list `[headers, codes, state]` and an optional, ignored `keys` list.

pub(crate) mod beacon_node_client;
pub(crate) mod engine_api_client;
pub(crate) mod validator;

use std::{
    collections::{HashMap, HashSet},
    fs,
    num::NonZeroUsize,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use alloy_primitives::B256;
use alloy_rlp::{Header, PayloadView};
use anyhow::{Context, anyhow, bail};
use axum::http::HeaderValue;
use beacon_node_client::{BEACON_NODE_RETRY_DELAY, BeaconNodeClient, ExecutionPayloadEvent};
use bytes::Bytes;
use engine_api_client::{EngineApiClient, EngineResponse};
use lighthouse_bls::Keypair;
use lighthouse_eth2_keystore::Keystore;
use lighthouse_types::{ChainSpec, Slot};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{mpsc, mpsc::error::TrySendError};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, debug, error, info, info_span, warn};
use validator::Validator;
use zkboost_types::{
    ExecutionWitness, Hash256, NewPayloadParams, NewPayloadRequest, ProgressiveList, ProofType,
    SszList,
};

use crate::{
    config::Config,
    dashboard::DashboardMessage,
    metrics::record_witness_fetch,
    proof::{
        input::{NewPayloadRequestMeta, StatelessInput},
        worker::{ProofResult, WorkerInput, WorkerOutput},
    },
};

/// JSON-RPC error code of a method the EL does not serve.
const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;
/// Proofs kept per proof type. Two epochs cover a payload imported again after a reorg.
const PROOF_CACHE_SLOTS: usize = 64;
/// Blocks kept from the `execution_payload` events, two epochs as the proof cache.
const BLOCK_CACHE_SLOTS: usize = 64;
/// Attempts to submit a proof, `BEACON_NODE_RETRY_DELAY` apart, which span less than the 12 second
/// slot.
const SUBMISSION_ATTEMPTS: u32 = 5;

/// JSON-RPC request envelope.
#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: Value,
    id: Value,
    method: String,
    params: Value,
}

/// JSON-RPC response envelope.
#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcResponse<T> {
    jsonrpc: Value,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

/// JSON-RPC error object. Every field but the code passes through unchanged.
#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcError {
    code: i64,
    #[serde(flatten)]
    other: Map<String, Value>,
}

/// Result of `engine_newPayloadWithWitnessV5`. The witness is not serialized, so the CL receives
/// the result of `engine_newPayloadV5`. Every other field passes through unchanged.
#[derive(Debug, Serialize, Deserialize)]
struct NewPayloadWithWitnessResponse {
    status: String,
    #[serde(skip_serializing)]
    witness: Option<alloy_primitives::Bytes>,
    #[serde(flatten)]
    other: Map<String, Value>,
}

/// An `engine_newPayloadV5` request of the CL.
#[derive(Debug)]
pub(crate) struct NewPayload {
    request: JsonRpcRequest,
    params: NewPayloadParams,
}

impl NewPayload {
    /// Decodes a new payload request, or returns `None` for every other request body.
    pub(crate) fn decode(body: &[u8]) -> Option<Self> {
        let request = serde_json::from_slice::<JsonRpcRequest>(body).ok()?;
        match NewPayloadParams::decode(&request.method, request.params.clone())? {
            Ok(params) => Some(Self { request, params }),
            Err(error) => {
                warn!(%error, "new payload params not decodable, forwarded as is");
                None
            }
        }
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
    proofs: Mutex<LruCache<(Hash256, ProofType), Vec<u8>>>,
    requested: Mutex<HashSet<(Hash256, ProofType)>>,
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
            .expect("config validation requires a zkvm");
        Ok(Self {
            engine_api_client: EngineApiClient::new(config.el_engine_endpoint.clone()),
            beacon_node_client: BeaconNodeClient::new(config.cl_beacon_endpoint.clone())?,
            keypair,
            validator: OnceLock::new(),
            blocks: Mutex::new(LruCache::new(
                NonZeroUsize::new(BLOCK_CACHE_SLOTS).expect("BLOCK_CACHE_SLOTS is not zero"),
            )),
            proofs: Mutex::new(LruCache::new(proofs_capacity)),
            requested: Mutex::new(HashSet::new()),
            worker_input_txs,
            dashboard_service_tx,
        })
    }

    /// Waits until the beacon node answers the startup reads. It then keeps the block cache from
    /// the events and records every proof attempt of the workers until the shutdown.
    pub(crate) async fn run(
        self: Arc<Self>,
        shutdown: CancellationToken,
        mut worker_output_rx: mpsc::Receiver<WorkerOutput>,
    ) {
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

    /// Forwards a new payload as `engine_newPayloadWithWitnessV5` and answers without the witness.
    /// Without the method or before the validator is ready, the payload is forwarded unchanged.
    pub(crate) async fn new_payload(
        self: Arc<Self>,
        authorization: Option<&HeaderValue>,
        new_payload: NewPayload,
        body: Bytes,
    ) -> reqwest::Result<EngineResponse> {
        let NewPayload { request, params } = new_payload;
        let block_hash = params.block_hash();
        info!(%block_hash, block_number = params.block_number(), "received new payload");
        if let Err(error) = self.validator() {
            warn!(%block_hash, %error, "payload not proven");
            return self.engine_api_client.forward(authorization, body).await;
        }

        let (response, result) = self
            .forward_new_payload(authorization, request, body, &params)
            .await?;
        if let Some(result) = result {
            if result.status == "VALID" {
                self.spawn_request_proofs(params, result.witness.map(|witness| witness.0));
            } else {
                debug!(%block_hash, status = result.status, "payload not valid, skip proving");
            }
        }
        Ok(response)
    }

    /// Forwards the payload as `engine_newPayloadWithWitnessV5` and records the round trip to the
    /// EL on the dashboard and the metrics. It returns the answer for the CL without the witness,
    /// and the parsed result. Without the method, the EL answers the plain request instead.
    async fn forward_new_payload(
        &self,
        authorization: Option<&HeaderValue>,
        request: JsonRpcRequest,
        body: Bytes,
        params: &NewPayloadParams,
    ) -> reqwest::Result<(EngineResponse, Option<NewPayloadWithWitnessResponse>)> {
        let block_hash = params.block_hash();
        self.notify_dashboard(DashboardMessage::fetch_witness_start(
            params.block_number(),
            block_hash,
            params.timestamp(),
            params.gas_used(),
        ));

        let upstream_request = JsonRpcRequest {
            method: NewPayloadParams::WITH_WITNESS_METHOD.to_owned(),
            ..request
        };
        let upstream_body = serde_json::to_vec(&upstream_request).unwrap();

        let started = Instant::now();
        let upstream_response = self
            .engine_api_client
            .forward(authorization, upstream_body.into())
            .await;
        let duration = started.elapsed();

        let record_fetch = |status: &'static str, witness_size: usize| {
            record_witness_fetch(status, duration, witness_size);
            self.notify_dashboard(DashboardMessage::fetch_witness_end(
                block_hash,
                witness_size,
                status == "success",
            ));
        };

        let upstream_response = upstream_response.inspect_err(|_| record_fetch("error", 0))?;
        let Ok(upstream_response_body) = serde_json::from_slice::<
            JsonRpcResponse<NewPayloadWithWitnessResponse>,
        >(&upstream_response.body) else {
            record_fetch("error", 0);
            return Ok((upstream_response, None));
        };

        if upstream_response_body
            .error
            .as_ref()
            .is_some_and(|error| error.code == JSON_RPC_METHOD_NOT_FOUND)
        {
            warn!(%block_hash, method = upstream_request.method, "witness method not served by the el, forwarded without proving");
            record_fetch("missing", 0);
            let upstream_response_body =
                self.engine_api_client.forward(authorization, body).await?;
            return Ok((upstream_response_body, None));
        }

        let witness = upstream_response_body
            .result
            .as_ref()
            .and_then(|result| result.witness.as_ref());
        match witness {
            Some(witness) => record_fetch("success", witness.len()),
            None => record_fetch("missing", 0),
        }

        Ok((
            EngineResponse {
                status: upstream_response.status,
                body: serde_json::to_vec(&upstream_response_body).unwrap().into(),
            },
            upstream_response_body.result,
        ))
    }

    /// Requests the proofs of a valid payload in the background under the `request_proof` span.
    fn spawn_request_proofs(self: &Arc<Self>, params: NewPayloadParams, witness: Option<Bytes>) {
        let block_hash = params.block_hash();
        let span = info_span!(
            "request_proof",
            %block_hash,
            block_number = params.block_number(),
            timestamp = params.timestamp(),
            gas_used = params.gas_used()
        );
        let state = self.clone();
        tokio::spawn(
            async move {
                if let Err(error) = state.request_proofs(params, witness, Span::current()).await {
                    error!(%block_hash, %error, "proof request failed");
                }
            }
            .instrument(span),
        );
    }

    /// Submits the cached proofs of a payload, then builds the stateless input from the witness
    /// and queues the rest at the zkVM workers.
    async fn request_proofs(
        self: &Arc<Self>,
        params: NewPayloadParams,
        witness: Option<Bytes>,
        span: Span,
    ) -> anyhow::Result<()> {
        let chain_id = self.validator()?.chain_id();
        let (payload_meta, payload) = tokio::task::spawn_blocking(move || {
            let payload = NewPayloadRequest::try_from(params)?;
            anyhow::Ok((NewPayloadRequestMeta::new(&payload)?, payload))
        })
        .await
        .expect("new payload request conversion does not panic")?;

        let mut proving = Vec::new();
        for (&proof_type, worker_input_tx) in &self.worker_input_txs {
            let proof = self
                .proofs
                .lock()
                .unwrap()
                .get(&(payload_meta.new_payload_request_root, proof_type))
                .cloned();
            let Some(proof) = proof else {
                proving.push((proof_type, worker_input_tx));
                continue;
            };
            info!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, "proof reused");
            self.submit_proof(payload_meta, proof_type, proof);
        }
        if proving.is_empty() {
            return Ok(());
        }

        let Some(witness) = witness else {
            bail!("valid payload without witness");
        };
        let stateless_input = tokio::task::spawn_blocking(move || {
            let witness = decode_engine_witness(&witness)?;
            StatelessInput::new(payload_meta, payload, witness, chain_id)
        })
        .await
        .expect("stateless input construction does not panic")
        .context("stateless input construction")?;
        let stateless_input = Arc::new(stateless_input);

        for (proof_type, worker_input_tx) in proving {
            let request = (payload_meta.new_payload_request_root, proof_type);
            if !self.requested.lock().unwrap().insert(request) {
                debug!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, "proof already requested");
                continue;
            }
            let worker_input = WorkerInput {
                stateless_input: stateless_input.clone(),
                span: span.clone(),
                queued_at: Instant::now(),
            };
            match worker_input_tx.try_send(worker_input) {
                Ok(()) => {
                    debug!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, "proof dispatched")
                }
                Err(error) => {
                    let reason = match error {
                        TrySendError::Full(_) => "worker channel full",
                        TrySendError::Closed(_) => "worker channel closed",
                    };
                    error!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, reason, "proof dispatch failed");
                    self.requested.lock().unwrap().remove(&request);
                }
            }
        }
        Ok(())
    }

    /// Records the outcome of a proof attempt and submits a proof to the beacon node.
    fn complete_proof(self: &Arc<Self>, output: WorkerOutput) {
        let WorkerOutput {
            payload_meta,
            proof_type,
            proof_result,
        } = output;
        match proof_result {
            ProofResult::Ok(proof) => {
                info!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, proof_size = proof.len(), "proved");
                self.proofs.lock().unwrap().put(
                    (payload_meta.new_payload_request_root, proof_type),
                    proof.clone(),
                );
                self.submit_proof(payload_meta, proof_type, proof);
            }
            ProofResult::Err(error) => {
                error!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, %error, "proving failed");
            }
            ProofResult::Timeout => {
                error!(block_hash = %payload_meta.block_hash, block_number = payload_meta.block_number, %proof_type, "proving timed out");
            }
        }
        self.requested
            .lock()
            .unwrap()
            .remove(&(payload_meta.new_payload_request_root, proof_type));
    }

    /// Submits a proof to the beacon node in the background, with `SUBMISSION_ATTEMPTS` attempts
    /// `BEACON_NODE_RETRY_DELAY` apart.
    fn submit_proof(
        self: &Arc<Self>,
        payload_meta: NewPayloadRequestMeta,
        proof_type: ProofType,
        proof: Vec<u8>,
    ) {
        let state = self.clone();
        let slot = Slot::new(payload_meta.slot);
        tokio::spawn(async move {
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
                        break;
                    }
                    Err(error) if attempt < SUBMISSION_ATTEMPTS => {
                        warn!(block_hash = %payload_meta.block_hash, %proof_type, attempt, error = %format!("{error:#}"), "proof submission retried");
                        tokio::time::sleep(BEACON_NODE_RETRY_DELAY).await;
                    }
                    Err(error) => {
                        error!(block_hash = %payload_meta.block_hash, %proof_type, error = %format!("{error:#}"), "proof submission failed");
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
    use alloy_rlp::Header;

    use crate::engine::decode_engine_witness;

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

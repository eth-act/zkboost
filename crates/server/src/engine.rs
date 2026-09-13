//! Engine API proxy state. Every JSON-RPC request is forwarded to the EL engine endpoint with the
//! caller's `Authorization` header unchanged, so the EL keeps validating the JWT of the CL.
//! `engine_newPayloadV5` is sent upstream as `engine_newPayloadWithWitnessV5`. The witness is
//! removed from the payload status returned to the CL, a `VALID` payload is proven by every zkVM
//! worker, and each proof is submitted to the beacon node as a signed EIP-8025 envelope.
//!
//! The EL returns the witness as the RLP list `[headers, codes, state]`, where `headers` holds
//! RLP-encoded block headers and the other two hold byte strings. Some clients append a trailing
//! `keys` list, which is ignored.

pub(crate) mod submission;

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use alloy_primitives::Bytes as AlloyBytes;
use alloy_rlp::{Header, PayloadView};
use anyhow::Context;
use axum::http::{HeaderValue, StatusCode};
use bytes::Bytes;
use lru::LruCache;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use submission::ProofSubmitter;
use tokio::sync::{OnceCell, mpsc, mpsc::error::TrySendError};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, debug, error, info, info_span, warn};
use url::Url;
use zkboost_types::{
    ExecutionWitness, Hash256, NewPayloadParams, NewPayloadRequest, ProgressiveList, ProofType,
    SszList,
};

use crate::{
    config::Config,
    dashboard::DashboardMessage,
    metrics::{record_prove, record_witness_fetch},
    proof::{
        input::StatelessInput,
        worker::{ProofResult, WorkerInput, WorkerOutput},
    },
};

/// JSON-RPC error code of a method the EL does not serve.
const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;
/// Proofs kept per proof type, two epochs of payloads, so a payload imported again after a reorg
/// is submitted without proving.
const PROOF_CACHE_SLOTS: usize = 64;

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
struct JsonRpcResponse {
    jsonrpc: Value,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

/// Raw HTTP response of the EL engine endpoint.
#[derive(Debug)]
pub(crate) struct EngineResponse {
    pub(crate) status: StatusCode,
    pub(crate) body: Bytes,
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

/// Client of the EL engine endpoint with the proving pipeline of every valid payload.
#[allow(missing_debug_implementations)]
pub(crate) struct EngineProxyState {
    url: Url,
    http_client: reqwest::Client,
    chain_id: OnceCell<u64>,
    proofs: Mutex<LruCache<(Hash256, ProofType), Vec<u8>>>,
    requested: Mutex<HashSet<(Hash256, ProofType)>>,
    submitter: Arc<ProofSubmitter>,
    worker_input_txs: HashMap<ProofType, mpsc::Sender<WorkerInput>>,
    dashboard_service_tx: mpsc::Sender<DashboardMessage>,
}

impl EngineProxyState {
    /// Creates the proxy of the configured EL with the input channel of every zkVM worker.
    pub(crate) fn new(
        config: &Config,
        worker_input_txs: HashMap<ProofType, mpsc::Sender<WorkerInput>>,
        dashboard_service_tx: mpsc::Sender<DashboardMessage>,
    ) -> anyhow::Result<Self> {
        let proofs_capacity = NonZeroUsize::new(PROOF_CACHE_SLOTS * worker_input_txs.len())
            .expect("config validation requires a zkvm");
        Ok(Self {
            url: config.el_engine_endpoint.clone(),
            http_client: reqwest::Client::new(),
            chain_id: OnceCell::new(),
            proofs: Mutex::new(LruCache::new(proofs_capacity)),
            requested: Mutex::new(HashSet::new()),
            submitter: Arc::new(ProofSubmitter::new(config)?),
            worker_input_txs,
            dashboard_service_tx,
        })
    }

    /// Forwards a JSON-RPC request body with the caller's `Authorization` header.
    pub(crate) async fn forward(
        &self,
        authorization: Option<&HeaderValue>,
        body: Bytes,
    ) -> reqwest::Result<EngineResponse> {
        let mut request = self
            .http_client
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization.as_bytes());
        }
        let response = request.send().await?;
        Ok(EngineResponse {
            status: response.status(),
            body: response.bytes().await?,
        })
    }

    /// Forwards a new payload as `engine_newPayloadWithWitnessV5`, answers with the
    /// payload status without the witness, and proves a `VALID` payload in the background. An EL
    /// without the witness method receives the original request instead.
    pub(crate) async fn new_payload(
        self: Arc<Self>,
        authorization: Option<&HeaderValue>,
        new_payload: NewPayload,
        body: Bytes,
    ) -> reqwest::Result<EngineResponse> {
        let NewPayload { request, params } = new_payload;
        let payload = params.execution_payload_v1();
        let block_hash = payload.block_hash;
        let block_number = payload.block_number;
        info!(%block_hash, block_number, "received new payload");
        self.notify_dashboard(DashboardMessage::request_proof(
            block_number,
            block_hash,
            payload.timestamp,
            payload.gas_used,
            self.worker_input_txs.keys().copied(),
        ));
        self.notify_dashboard(DashboardMessage::fetch_witness_start(block_hash));

        let upstream_request = JsonRpcRequest {
            method: NewPayloadParams::WITH_WITNESS_METHOD.to_owned(),
            ..request
        };
        let upstream_body = serde_json::to_vec(&upstream_request).expect("request is serializable");
        let fetch_started = Instant::now();
        let upstream = self
            .forward(authorization, upstream_body.into())
            .await
            .inspect_err(|_| self.record_witness_fetch(block_hash, "error", Duration::ZERO, 0))?;
        let fetch_duration = fetch_started.elapsed();
        let Ok(mut response) = serde_json::from_slice::<JsonRpcResponse>(&upstream.body) else {
            self.record_witness_fetch(block_hash, "error", fetch_duration, 0);
            return Ok(upstream);
        };
        let error_code = response
            .error
            .as_ref()
            .and_then(|error| error.get("code"))
            .and_then(Value::as_i64);
        if error_code == Some(JSON_RPC_METHOD_NOT_FOUND) {
            warn!(%block_hash, method = upstream_request.method, "witness method not served by the el, forwarded without proving");
            self.record_witness_fetch(block_hash, "missing", fetch_duration, 0);
            return self.forward(authorization, body).await;
        }
        let witness = response
            .result
            .as_mut()
            .and_then(Value::as_object_mut)
            .and_then(|status| status.remove("witness"))
            .and_then(|witness| {
                serde_json::from_value::<AlloyBytes>(witness)
                    .inspect_err(|error| warn!(%block_hash, %error, "witness not decodable"))
                    .ok()
            });
        let status = response
            .result
            .as_ref()
            .and_then(|status| status.get("status"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (fetch_status, witness_size) = match &witness {
            Some(witness) => ("success", witness.len()),
            None => ("missing", 0),
        };
        self.record_witness_fetch(block_hash, fetch_status, fetch_duration, witness_size);

        match (status, witness) {
            ("VALID", Some(witness)) => {
                let span = info_span!(
                    "request_proof",
                    %block_hash,
                    block_number,
                    timestamp = payload.timestamp,
                    gas_used = payload.gas_used
                );
                let state = self.clone();
                let authorization = authorization.cloned();
                tokio::spawn(
                    async move {
                        if let Err(error) = state
                            .request_proofs(authorization, params, witness, Span::current())
                            .await
                        {
                            error!(%block_hash, %error, "proof request failed");
                        }
                    }
                    .instrument(span),
                );
            }
            ("VALID", None) => warn!(%block_hash, "valid payload without witness, not proving"),
            (status, _) => debug!(%block_hash, status, "payload not valid, not proving"),
        }

        Ok(EngineResponse {
            status: upstream.status,
            body: serde_json::to_vec(&response)
                .expect("response is serializable")
                .into(),
        })
    }

    /// Builds the stateless input of a valid payload, submits the cached proof of every proof type
    /// that proved it before, and queues it at every other zkVM worker that has not received it.
    async fn request_proofs(
        &self,
        authorization: Option<HeaderValue>,
        params: NewPayloadParams,
        witness: AlloyBytes,
        span: Span,
    ) -> anyhow::Result<()> {
        let chain_id = self
            .chain_id(authorization.as_ref())
            .await
            .context("chain id resolution")?;
        let stateless_input = tokio::task::spawn_blocking(move || {
            let witness = decode_engine_witness(&witness)?;
            StatelessInput::new(NewPayloadRequest::try_from(params)?, witness, chain_id)
        })
        .await
        .expect("stateless input construction does not panic")
        .context("stateless input construction")?;
        let stateless_input = Arc::new(stateless_input);
        let root = stateless_input.root();
        let block_hash = stateless_input.block_hash();
        let parent_beacon_block_root = stateless_input.parent_beacon_block_root();
        let block_number = stateless_input.block_number();

        for (&proof_type, worker_input_tx) in &self.worker_input_txs {
            let proof = self
                .proofs
                .lock()
                .unwrap()
                .get(&(root, proof_type))
                .cloned();
            if let Some(proof) = proof {
                info!(%block_hash, block_number, %proof_type, "proof reused");
                self.notify_dashboard(DashboardMessage::prove_start(block_hash, proof_type));
                self.notify_dashboard(DashboardMessage::prove_end(
                    block_hash,
                    proof_type,
                    &ProofResult::Ok(proof.clone()),
                ));
                self.submit_proof(block_hash, parent_beacon_block_root, proof_type, proof);
                continue;
            }
            if !self.requested.lock().unwrap().insert((root, proof_type)) {
                debug!(%block_hash, block_number, %proof_type, "proof already requested");
                continue;
            }
            let worker_input = WorkerInput {
                stateless_input: stateless_input.clone(),
                span: span.clone(),
                queued_at: Instant::now(),
            };
            match worker_input_tx.try_send(worker_input) {
                Ok(()) => debug!(%block_hash, block_number, %proof_type, "proof dispatched"),
                Err(error) => {
                    let reason = match error {
                        TrySendError::Full(_) => "worker channel full",
                        TrySendError::Closed(_) => "worker channel closed",
                    };
                    error!(%block_hash, block_number, %proof_type, reason, "proof dispatch failed");
                    self.requested.lock().unwrap().remove(&(root, proof_type));
                    record_prove(proof_type, "error", Duration::ZERO, 0);
                }
            }
        }
        Ok(())
    }

    /// Records the outcome of every proof attempt of the workers until shutdown.
    pub(crate) async fn complete_proofs(
        self: Arc<Self>,
        shutdown: CancellationToken,
        mut worker_output_rx: mpsc::Receiver<WorkerOutput>,
    ) {
        loop {
            tokio::select! {
                biased;

                _ = shutdown.cancelled() => break,

                output = worker_output_rx.recv() => match output {
                    Some(output) => self.complete_proof(output),
                    None => break,
                },
            }
        }
    }

    /// Records the outcome of a proof attempt and submits a proof to the beacon node.
    fn complete_proof(&self, output: WorkerOutput) {
        let WorkerOutput {
            new_payload_request_root,
            block_hash,
            parent_beacon_block_root,
            block_number,
            proof_type,
            proof_result,
            duration,
        } = output;
        self.requested
            .lock()
            .unwrap()
            .remove(&(new_payload_request_root, proof_type));
        self.notify_dashboard(DashboardMessage::prove_end(
            block_hash,
            proof_type,
            &proof_result,
        ));
        match proof_result {
            ProofResult::Ok(proof) => {
                info!(%block_hash, block_number, %proof_type, proof_size = proof.len(), "proved");
                record_prove(proof_type, "success", duration, proof.len());
                self.proofs
                    .lock()
                    .unwrap()
                    .put((new_payload_request_root, proof_type), proof.clone());
                self.submit_proof(block_hash, parent_beacon_block_root, proof_type, proof);
            }
            ProofResult::Err(error) => {
                error!(%block_hash, block_number, %proof_type, %error, "proving failed");
                record_prove(proof_type, "error", duration, 0);
            }
            ProofResult::Timeout => {
                error!(%block_hash, block_number, %proof_type, "proving timed out");
                record_prove(proof_type, "timeout", duration, 0);
            }
        }
    }

    /// Submits a proof to the beacon node in the background.
    fn submit_proof(
        &self,
        block_hash: Hash256,
        parent_beacon_block_root: Hash256,
        proof_type: ProofType,
        proof: Vec<u8>,
    ) {
        let submitter = self.submitter.clone();
        tokio::spawn(async move {
            match submitter
                .submit(
                    block_hash,
                    parent_beacon_block_root,
                    proof_type.execution_proof_type(),
                    proof,
                )
                .await
            {
                Ok(()) => info!(%block_hash, %proof_type, "proof submitted"),
                Err(error) => error!(%block_hash, %proof_type, %error, "proof submission failed"),
            }
        });
    }

    /// Sends a message to the dashboard service, which is absent when the dashboard is disabled.
    fn notify_dashboard(&self, message: DashboardMessage) {
        let _ = self.dashboard_service_tx.try_send(message);
    }

    /// Returns the chain id of the EL, read with `eth_chainId` on the first call.
    async fn chain_id(&self, authorization: Option<&HeaderValue>) -> anyhow::Result<u64> {
        self.chain_id
            .get_or_try_init(|| async {
                #[derive(Deserialize)]
                struct Response {
                    result: Option<alloy_primitives::U64>,
                    error: Option<Value>,
                }
                let body = br#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#;
                let response = self
                    .forward(authorization, Bytes::from_static(body))
                    .await
                    .context("eth_chainId")?;
                let response: Response =
                    serde_json::from_slice(&response.body).context("eth_chainId response")?;
                if let Some(error) = response.error {
                    anyhow::bail!("eth_chainId failed: {error}");
                }
                Ok(response
                    .result
                    .context("eth_chainId response has no result")?
                    .to())
            })
            .await
            .copied()
    }

    fn record_witness_fetch(
        &self,
        block_hash: Hash256,
        status: &'static str,
        duration: Duration,
        witness_size: usize,
    ) {
        record_witness_fetch(status, duration, witness_size);
        self.notify_dashboard(DashboardMessage::fetch_witness_end(
            block_hash,
            witness_size,
            status == "success",
        ));
    }
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

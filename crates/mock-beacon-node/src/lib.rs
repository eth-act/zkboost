//! A mock of a beacon node with the EIP-8025 behavior. It serves the Engine API to a CL and
//! forwards every request to zkboost, so zkboost sees the Engine API traffic of a real CL. It
//! accepts the proofs of every valid `engine_newPayloadV5` payload for `PROOF_TIMEOUT`, and
//! verifies the signature and the proof of each as a beacon node does. Every other beacon API
//! request goes to the CL.

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::B256;
use anyhow::{Context, bail, ensure};
use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{
        HeaderMap, StatusCode,
        header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, TRANSFER_ENCODING},
    },
    response::{IntoResponse, Response},
    routing::post,
};
use ere_verifier::Verifier;
use lighthouse_bls::{PublicKey, Signature};
use lighthouse_types::{ChainSpec, EthSpec, MainnetEthSpec, Slot};
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use tokio::{net::TcpListener, sync::OnceCell, time::sleep};
use tracing::{info, warn};
use url::Url;
use zkboost_server::{
    beacon::{
        beacon_client::BeaconClient,
        execution_proof::{
            MAX_EXECUTION_PROOFS_PER_PAYLOAD, MAX_PROOF_SIZE, MockProof,
            SignedExecutionProofEnvelope, SignedExecutionProofEnvelopes,
        },
    },
    engine::payload::{PayloadHeader, decode_new_payload_request, public_values},
    proof::proof_type::ProofType,
    stateless_validator_common::{SszDecode, guest::input::ProtocolFork},
};

/// Budget for the proofs of one payload to arrive.
const PROOF_TIMEOUT: Duration = Duration::from_secs(60);

/// The number of verified proofs of one payload below which the payload logs a warning.
const MIN_VERIFIED_PROOFS: usize = 2;

/// The default `proof-engine.json`, with the verifying key of every proof type.
pub const DEFAULT_PROOF_ENGINE_JSON: &str = include_str!("../tests/fixture/proof-engine.json");

/// A valid payload of the CL, whose proofs are accepted.
struct Pending {
    slot: u64,
    /// The public values that the guest commits for the payload.
    public_values: Vec<u8>,
    /// The proof types of the verified proofs.
    verified_proof_types: HashSet<u8>,
}

pub struct MockBeaconNode {
    cl: BeaconClient,
    cl_endpoint: Url,
    zkboost_endpoint: Url,
    http: reqwest::Client,
    spec: OnceCell<ChainSpec>,
    genesis_validators_root: OnceCell<B256>,
    verifiers: HashMap<u8, Verifier>,
    /// The valid payloads that accept proofs, by block hash.
    pending_payloads: Mutex<HashMap<B256, Pending>>,
}

#[derive(Deserialize)]
struct JsonRpcRequest<'a> {
    method: &'a str,
    #[serde(borrow)]
    params: &'a RawValue,
}

#[derive(Deserialize)]
struct NewPayloadResponse {
    result: Option<PayloadStatus>,
}

#[derive(Deserialize)]
struct PayloadStatus {
    status: String,
}

#[derive(Deserialize)]
struct ValidatorData {
    validator: ValidatorPubkey,
}

#[derive(Deserialize)]
struct ValidatorPubkey {
    #[serde(with = "serde_utils::hex_vec")]
    pubkey: Vec<u8>,
}

/// The `proof-engine.json` of the eth-act Lighthouse.
#[derive(Deserialize)]
struct ProofEngine {
    execution_proofs: Vec<ProofEngineEntry>,
}

#[derive(Deserialize)]
struct ProofEngineEntry {
    proof_type: u8,
    #[serde(with = "serde_utils::hex_vec")]
    program_vk: Vec<u8>,
}

impl MockBeaconNode {
    /// Builds the mock with the verifier of every proof type of the `proof-engine.json`.
    pub fn new(
        cl_endpoint: Url,
        zkboost_endpoint: Url,
        proof_engine_json: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            cl: BeaconClient::new(cl_endpoint.clone()),
            cl_endpoint,
            zkboost_endpoint,
            http: (reqwest::Client::builder().redirect(Policy::none())).build()?,
            spec: OnceCell::new(),
            genesis_validators_root: OnceCell::new(),
            verifiers: verifiers(proof_engine_json)?,
            pending_payloads: Mutex::default(),
        })
    }

    pub async fn serve(
        self: Arc<Self>,
        beacon_api: TcpListener,
        engine_api: TcpListener,
    ) -> io::Result<()> {
        info!(beacon_api = %beacon_api.local_addr()?, engine_api = %engine_api.local_addr()?, "listening");
        let max_submission_size = MAX_EXECUTION_PROOFS_PER_PAYLOAD * (MAX_PROOF_SIZE + 1024);
        let beacon = Router::new()
            .route(
                "/eth/v1/beacon/execution_proofs",
                post(Self::execution_proofs),
            )
            .route_layer(DefaultBodyLimit::max(max_submission_size))
            .fallback(Self::forward_to_cl)
            .with_state(self.clone());
        let engine = Router::new()
            .route("/", post(Self::forward_to_zkboost))
            // The limit of the Engine API of zkboost.
            .layer(DefaultBodyLimit::max(128 << 20))
            .with_state(self);
        tokio::try_join!(
            axum::serve(beacon_api, beacon),
            axum::serve(engine_api, engine)
        )?;
        Ok(())
    }

    async fn spec(&self) -> anyhow::Result<&ChainSpec> {
        self.spec.get_or_try_init(|| self.cl.spec()).await
    }

    /// Handles `POST /` of the Engine API. It returns the response of zkboost to the
    /// unchanged request, and 502 without one.
    async fn forward_to_zkboost(
        State(node): State<Arc<Self>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        // The headers of the CL go to zkboost, without the ones of the connection.
        let mut headers = headers;
        for name in [HOST, CONTENT_LENGTH, CONNECTION, TRANSFER_ENCODING] {
            headers.remove(name);
        }
        let request = (node.http.post(node.zkboost_endpoint.clone()))
            .headers(headers)
            .body(body.clone());
        let (status, response_body) = match async {
            let response = request.send().await?;
            reqwest::Result::Ok((response.status(), response.bytes().await?))
        }
        .await
        {
            Ok(response) => response,
            Err(error) => {
                warn!(%error, "forward to zkboost failed");
                return StatusCode::BAD_GATEWAY.into_response();
            }
        };
        if let Err(error) = node.track_valid_payload(&body, &response_body).await {
            warn!(error = %format!("{error:#}"), "new payload not awaited");
        }
        (status, [(CONTENT_TYPE, "application/json")], response_body).into_response()
    }

    /// Accepts the proofs of the payload of an `engine_newPayloadV5` that got `VALID` from
    /// zkboost, until `PROOF_TIMEOUT`.
    async fn track_valid_payload(
        self: &Arc<Self>,
        request: &[u8],
        response_body: &[u8],
    ) -> anyhow::Result<()> {
        let Ok(JsonRpcRequest {
            method: "engine_newPayloadV5",
            params,
        }) = serde_json::from_slice(request)
        else {
            return Ok(());
        };
        let response: NewPayloadResponse =
            serde_json::from_slice(response_body).context("decode response")?;
        let header: PayloadHeader = serde_json::from_str(params.get()).context("decode params")?;
        let block_hash = header.block_hash;
        let status = response.result.map(|result| result.status);
        if status.as_deref() != Some("VALID") {
            info!(%block_hash, ?status, "payload not valid");
            return Ok(());
        }
        let new_payload_request = decode_new_payload_request(params.get().as_bytes())?;
        let chain_id = self.spec().await?.deposit_chain_id;
        let pending = Pending {
            slot: header.slot,
            // zkboost proves every engine_newPayloadV5 payload as Amsterdam.
            public_values: public_values(&new_payload_request, ProtocolFork::Amsterdam, chain_id),
            verified_proof_types: HashSet::new(),
        };
        match self.pending_payloads.lock().unwrap().entry(block_hash) {
            Entry::Vacant(entry) => entry.insert(pending),
            Entry::Occupied(_) => return Ok(()),
        };
        info!(%block_hash, "valid payload forwarded");
        let node = self.clone();
        tokio::spawn(async move {
            sleep(PROOF_TIMEOUT).await;
            let pending = node.pending_payloads.lock().unwrap().remove(&block_hash);
            let verified_proof_types =
                (pending.map(|pending| pending.verified_proof_types)).unwrap_or_default();
            if verified_proof_types.len() < MIN_VERIFIED_PROOFS {
                warn!(%block_hash, ?verified_proof_types, "proofs not verified in time");
            }
        });
        Ok(())
    }

    /// Handles `POST /eth/v1/beacon/execution_proofs` with an SSZ body. A rejected envelope
    /// fails the request with 400 and its index, as the beacon node responds.
    async fn execution_proofs(State(node): State<Arc<Self>>, body: Bytes) -> Response {
        let Ok(envelopes) = SignedExecutionProofEnvelopes::from_ssz_bytes(&body) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let mut failures = Vec::new();
        for (index, envelope) in envelopes.iter().enumerate() {
            let block_root = B256::from(envelope.message.beacon_block_root);
            let proof_type = envelope.message.proof_type;
            match node.verify(envelope).await {
                Ok(()) => info!(%block_root, proof_type, "proof verified"),
                Err(error) => {
                    warn!(%block_root, proof_type, error = %format!("{error:#}"), "proof rejected");
                    failures.push(json!({ "index": index, "message": format!("{error:#}") }));
                }
            }
        }
        if failures.is_empty() {
            return StatusCode::OK.into_response();
        }
        let body = json!({ "code": 400, "message": "error processing execution proofs", "failures": failures });
        (StatusCode::BAD_REQUEST, Json(body)).into_response()
    }

    /// Verifies the signature of the envelope as the beacon node does, then the proof against
    /// the pending payload of the bid of its beacon block.
    async fn verify(&self, envelope: &SignedExecutionProofEnvelope) -> anyhow::Result<()> {
        let message = &envelope.message;
        let block_hash = self
            .cl
            .bid_block_hash(message.beacon_block_root.into())
            .await?;
        let pending_payload = |node: &Self| {
            let pending_payloads = node.pending_payloads.lock().unwrap();
            let pending = pending_payloads.get(&block_hash);
            pending.map(|pending| (pending.slot, pending.public_values.clone()))
        };
        let (slot, public_values) =
            pending_payload(self).with_context(|| format!("payload {block_hash} not pending"))?;

        let path = format!(
            "eth/v1/beacon/states/head/validators/{}",
            envelope.validator_index
        );
        let validator: ValidatorData = self.cl.get(&path).await?.context("validator not found")?;
        let pubkey = PublicKey::deserialize(&validator.validator.pubkey)
            .map_err(|error| anyhow::anyhow!("validator pubkey: {error:?}"))?;
        let signature = Signature::deserialize(&envelope.signature)
            .map_err(|error| anyhow::anyhow!("signature: {error:?}"))?;
        let epoch = Slot::new(slot).epoch(MainnetEthSpec::slots_per_epoch());
        let fork_version = self.spec().await?.fork_at_epoch(epoch).current_version;
        let genesis_validators_root = (self.genesis_validators_root)
            .get_or_try_init(|| self.cl.genesis_validators_root())
            .await?;
        let signing_root = message.signing_root(fork_version, *genesis_validators_root);
        ensure!(
            signature.verify(&pubkey, signing_root.0.into()),
            "invalid signature of validator {}",
            envelope.validator_index
        );

        self.verify_proof(message.proof_type, &message.proof_data, &public_values)?;
        if let Some(pending) = self.pending_payloads.lock().unwrap().get_mut(&block_hash) {
            pending.verified_proof_types.insert(message.proof_type);
        }
        Ok(())
    }

    /// Reads the public values of a mock proof of any proof type, otherwise verifies the proof
    /// with the verifier of its type, and checks the public values against the expected ones.
    fn verify_proof(&self, proof_type: u8, proof: &[u8], expected: &[u8]) -> anyhow::Result<()> {
        let public_values = match MockProof::from_ssz_bytes(proof) {
            Ok(mock_proof) => mock_proof.public_values,
            Err(_) => {
                let Some(verifier) = self.verifiers.get(&proof_type) else {
                    bail!("unsupported proof type {proof_type}")
                };
                verifier.verify(proof)?.to_vec()
            }
        };
        // A zkVM with fixed size public values pads the SSZ result with zeros.
        ensure!(
            public_values.starts_with(expected)
                && public_values[expected.len()..]
                    .iter()
                    .all(|byte| *byte == 0),
            "unexpected public values, expected {expected:?}, got {public_values:?}"
        );
        Ok(())
    }

    /// Forwards every other beacon API request to the CL, and streams the response.
    async fn forward_to_cl(State(node): State<Arc<Self>>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request
            .uri()
            .path_and_query()
            .map_or("", |path| path.as_str());
        // A path that is itself a URL replaces the CL in the join.
        let url = (node.cl_endpoint.join(path.trim_start_matches('/')).ok())
            .filter(|url| url.origin() == node.cl_endpoint.origin());
        let Ok(body) = to_bytes(request.into_body(), usize::MAX).await else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let Some(url) = url else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        match node.http.request(method, url).body(body).send().await {
            Ok(response) => {
                let status = response.status();
                let content_type = response.headers().get(CONTENT_TYPE).cloned();
                let headers = content_type.map(|content_type| [(CONTENT_TYPE, content_type)]);
                (status, headers, Body::from_stream(response.bytes_stream())).into_response()
            }
            Err(error) => {
                warn!(%error, "forward to cl failed");
                StatusCode::BAD_GATEWAY.into_response()
            }
        }
    }
}

fn verifiers(proof_engine_json: &str) -> anyhow::Result<HashMap<u8, Verifier>> {
    let proof_engine: ProofEngine =
        serde_json::from_str(proof_engine_json).context("decode proof-engine.json")?;
    let mut verifiers = HashMap::new();
    for ProofEngineEntry {
        proof_type: id,
        program_vk,
    } in proof_engine.execution_proofs
    {
        let proof_type =
            ProofType::from_repr(id).with_context(|| format!("unknown proof type {id}"))?;
        let verifier = Verifier::new(proof_type.zkvm_kind(), &program_vk)
            .with_context(|| format!("decode verifying key of {proof_type}"))?;
        ensure!(
            verifiers.insert(id, verifier).is_none(),
            "duplicate proof type {proof_type}"
        );
        info!(%proof_type, "verifier loaded");
    }
    Ok(verifiers)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use strum::IntoEnumIterator;
    use zkboost_server::{
        beacon::execution_proof::MockProof, proof::proof_type::ProofType,
        stateless_validator_common::SszEncode,
    };

    use crate::{DEFAULT_PROOF_ENGINE_JSON, MockBeaconNode};

    fn mock_beacon_node() -> MockBeaconNode {
        let endpoint: url::Url = "http://127.0.0.1:1".parse().unwrap();
        MockBeaconNode::new(endpoint.clone(), endpoint, DEFAULT_PROOF_ENGINE_JSON).unwrap()
    }

    /// A mock proof passes for any proof type with the public values of the payload, also with
    /// zero padding, and fails with other ones.
    #[test]
    fn test_mock_proof_verify() {
        let node = mock_beacon_node();
        for (public_values, valid) in [
            (vec![0xaa; 43], true),
            ([vec![0xaa; 43], vec![0; 21]].concat(), true),
            (vec![0xab; 43], false),
            (vec![0xaa; 42], false),
        ] {
            let proof = MockProof {
                public_values,
                proof: vec![0xbb; 1 << 10],
            };
            let verified = node.verify_proof(200, &proof.to_ssz(), &[0xaa; 43]);
            assert_eq!(verified.is_ok(), valid);
        }
    }

    /// The fixture proof of every proof type passes its verifier with the fixture public values.
    #[test]
    fn test_fixture_proofs_verify() {
        let node = mock_beacon_node();
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixture");
        let public_values = fs::read(format!("{fixture}/public_values.bin")).unwrap();
        for proof_type in ProofType::iter() {
            let version = proof_type.zkvm_kind().sdk_version();
            let path = format!("{fixture}/stateless-validator-{proof_type}-{version}.proof");
            let proof = fs::read(&path).unwrap();
            node.verify_proof(proof_type as u8, &proof, &public_values)
                .unwrap_or_else(|error| panic!("{path}: {error:#}"));
        }
    }
}

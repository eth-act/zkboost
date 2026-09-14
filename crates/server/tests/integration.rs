//! Integration test for zkboost. A mock EL answers `engine_newPayloadWithWitnessV5` with the
//! fixture witness. A mock beacon node resolves the beacon block of the fixture payload. It
//! collects every signed envelope of `POST /eth/v1/beacon/execution_proofs`.

use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use alloy_primitives::{B256, Bytes, hex};
use alloy_rlp::Header;
use alloy_rpc_types_engine::{Claims, JwtSecret};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::sse::{Event as SseEvent, Sse},
};
use lighthouse_bls::{PublicKey, Signature};
use lighthouse_eth2_keystore::Keystore;
use lighthouse_types::{ChainSpec, Config as SpecConfig, Epoch, MainnetEthSpec};
use metrics_exporter_prometheus::PrometheusBuilder;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Notify, mpsc},
};
use tokio_stream::{Stream, StreamExt};
use tracing_subscriber::EnvFilter;
use url::Url;
use zkboost_server::{
    config::{Config, DashboardConfig, MockProvingTime, zkVMConfig},
    server::zkBoostServer,
};
use zkboost_types::{
    ExecutionWitness, Hash256, HashTreeRoot, MOCK_PROOF, NewPayloadParams, ProofType, ProtocolFork,
    Sha2Hasher, SignedExecutionProofEnvelope, SignedExecutionProofEnvelopes, SszDecode,
    StatelessInput, StatelessValidationResult, execution_proof_domain,
};

/// The keystore of validator 256 of the ethereum-package mnemonic, a copy of the testnet example.
const VOTING_KEYSTORE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixture/voting-keystore.json"
);
const VOTING_KEYSTORE_PASSWORD_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixture/voting-keystore-password"
);
const VALIDATOR_INDEX: u64 = 256;
/// The beacon block of the fixture payload, as the mock beacon node reports it.
const BEACON_BLOCK_ROOT: B256 = B256::repeat_byte(0xbb);
/// Another child of the parent beacon block, without the fixture payload.
const OTHER_BLOCK_ROOT: B256 = B256::repeat_byte(0xcc);
const GENESIS_VALIDATORS_ROOT: B256 = B256::repeat_byte(0x99);
const FORK_VERSION: [u8; 4] = [0x10, 0x00, 0x00, 0x38];

/// The stateless input of block 93354 of glamsterdam-devnet-8.
const AMSTERDAM_STATELESS_INPUT: &[u8] = include_bytes!("fixture/stateless_input_amsterdam.ssz");

struct Fixture {
    params: NewPayloadParams,
    new_payload_request_root: Hash256,
    block_hash: B256,
    parent_beacon_block_root: B256,
    slot: u64,
    chain_id: u64,
    witness: Bytes,
}

impl Fixture {
    fn load() -> Self {
        let (_, input) =
            StatelessInput::from_schema_prefixed_ssz(AMSTERDAM_STATELESS_INPUT).unwrap();
        let StatelessInput {
            new_payload_request,
            witness,
            chain_id,
            ..
        } = input;
        let params = NewPayloadParams::try_from(&new_payload_request).unwrap();
        Fixture {
            block_hash: params.execution_payload_v1().block_hash,
            parent_beacon_block_root: params.2,
            slot: params.0.slot_number,
            params,
            new_payload_request_root: Hash256::from(
                new_payload_request.hash_tree_root(&Sha2Hasher),
            ),
            chain_id,
            witness: encode_engine_witness(&witness),
        }
    }
}

/// Encodes the witness as the EL returns it, an RLP list of the headers, codes, and state nodes.
fn encode_engine_witness(witness: &ExecutionWitness) -> Bytes {
    fn rlp_list(items: impl Iterator<Item = Vec<u8>>) -> Vec<u8> {
        let payload: Vec<u8> = items.flatten().collect();
        let mut out = Vec::new();
        Header {
            list: true,
            payload_length: payload.len(),
        }
        .encode(&mut out);
        out.extend(payload);
        out
    }
    fn rlp_strings(items: impl Iterator<Item = Vec<u8>>) -> Vec<u8> {
        rlp_list(items.map(|item| alloy_rlp::encode(item.as_slice())))
    }
    let headers = rlp_list(witness.headers.iter().map(|header| header.to_vec()));
    let codes = rlp_strings(witness.codes.iter().map(|code| code.to_vec()));
    let state = rlp_strings(witness.state.iter().map(|node| node.to_vec()));
    rlp_list([headers, codes, state].into_iter()).into()
}

/// A JSON-RPC call seen by the mock EL.
#[derive(Debug, Clone)]
struct ElCall {
    method: String,
    authorization: Option<String>,
}

struct MockElState {
    fixture: Fixture,
    payload_status: &'static str,
    witness_supported: bool,
    calls: Mutex<Vec<ElCall>>,
}

async fn mock_el_handler(
    State(state): State<Arc<MockElState>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Json<Value> {
    let method = request["method"].as_str().unwrap_or_default().to_owned();
    state.calls.lock().unwrap().push(ElCall {
        method: method.clone(),
        authorization: headers
            .get(AUTHORIZATION)
            .map(|value| value.to_str().unwrap().to_owned()),
    });

    if method == NewPayloadParams::WITH_WITNESS_METHOD && !state.witness_supported {
        return Json(json!({
            "jsonrpc": "2.0",
            "error": { "code": -32601, "message": "Method not found" },
            "id": request["id"],
        }));
    }
    let result = match method.as_str() {
        method if method == NewPayloadParams::WITH_WITNESS_METHOD => {
            let mut status = json!({
                "status": state.payload_status,
                "latestValidHash": state.fixture.block_hash,
                "validationError": null,
            });
            if state.payload_status == "VALID" {
                status["witness"] = json!(state.fixture.witness);
            }
            status
        }
        method if method == NewPayloadParams::METHOD => {
            json!({ "status": "VALID", "latestValidHash": state.fixture.block_hash, "validationError": null })
        }
        "engine_forkchoiceUpdatedV4" => json!({
            "payloadStatus": { "status": "VALID", "latestValidHash": null, "validationError": null },
            "payloadId": null,
        }),
        _ => Value::Null,
    };

    Json(json!({ "jsonrpc": "2.0", "result": result, "id": request["id"] }))
}

async fn start_mock_el(
    fixture: Fixture,
    payload_status: &'static str,
    witness_supported: bool,
) -> (Url, Arc<MockElState>) {
    let state = Arc::new(MockElState {
        fixture,
        payload_status,
        witness_supported,
        calls: Mutex::new(Vec::new()),
    });
    let app = axum::Router::new()
        .route("/", axum::routing::post(mock_el_handler))
        .with_state(state.clone());
    (serve(app).await, state)
}

struct MockBeaconNode {
    fixture: Fixture,
    ambiguous_block: bool,
    mismatched_slot: bool,
    block_from_event: bool,
    envelopes: mpsc::UnboundedSender<SignedExecutionProofEnvelope>,
    /// Signals `GET /eth/v1/events`, which zkboost opens once the validator is ready.
    events_opened: Notify,
}

async fn genesis_handler() -> Json<Value> {
    Json(json!({ "data": { "genesis_validators_root": GENESIS_VALIDATORS_ROOT } }))
}

/// `GET /eth/v1/beacon/headers?parent_root=`, which lists the fixture block under its parent.
/// Every header reports the fixture slot, or the next slot under `mismatched_slot`. A node that
/// announces the block on the event stream lists no child.
async fn headers_handler(
    State(node): State<Arc<MockBeaconNode>>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    let parent_root = query
        .get("parent_root")
        .and_then(|root| root.parse::<B256>().ok());
    let slot = (node.fixture.slot + u64::from(node.mismatched_slot)).to_string();
    let headers = if parent_root == Some(node.fixture.parent_beacon_block_root)
        && !node.block_from_event
    {
        json!([
            { "root": OTHER_BLOCK_ROOT, "canonical": false, "header": { "message": { "slot": slot } } },
            { "root": BEACON_BLOCK_ROOT, "canonical": true, "header": { "message": { "slot": slot } } },
        ])
    } else {
        json!([])
    };
    Json(json!({ "data": headers }))
}

/// `GET /eth/v2/beacon/blocks/{root}`, with the fixture payload bid under the fixture block only.
/// An ambiguous node answers that bid under both children of the parent.
async fn block_handler(
    State(node): State<Arc<MockBeaconNode>>,
    Path(root): Path<B256>,
) -> Result<Json<Value>, StatusCode> {
    let block_hash = match root {
        BEACON_BLOCK_ROOT => node.fixture.block_hash,
        OTHER_BLOCK_ROOT if node.ambiguous_block => node.fixture.block_hash,
        OTHER_BLOCK_ROOT => B256::ZERO,
        _ => return Err(StatusCode::NOT_FOUND),
    };
    Ok(Json(json!({ "data": { "message": { "body": {
        "signed_execution_payload_bid": { "message": { "block_hash": block_hash } }
    } } } })))
}

/// `GET /eth/v1/events`, which announces the fixture block under `block_from_event`, and the
/// other block as well under `ambiguous_block`. The stream stays open afterwards.
async fn events_handler(
    State(node): State<Arc<MockBeaconNode>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    node.events_opened.notify_one();
    let mut block_roots = Vec::new();
    if node.block_from_event {
        block_roots.push(BEACON_BLOCK_ROOT);
        if node.ambiguous_block {
            block_roots.push(OTHER_BLOCK_ROOT);
        }
    }
    let events: Vec<_> = block_roots
        .into_iter()
        .map(|block_root| {
            Ok(SseEvent::default()
                .event("execution_payload")
                .json_data(json!({
                    "slot": node.fixture.slot.to_string(),
                    "block_hash": node.fixture.block_hash,
                    "block_root": block_root,
                }))
                .unwrap())
        })
        .collect();
    Sse::new(tokio_stream::iter(events).chain(tokio_stream::pending()))
}

/// `GET /eth/v1/config/spec`, the mainnet spec with every fork at genesis.
async fn spec_handler(State(node): State<Arc<MockBeaconNode>>) -> Json<Value> {
    let spec = mock_spec(node.fixture.chain_id);
    Json(json!({ "data": SpecConfig::from_chain_spec::<MainnetEthSpec>(&spec) }))
}

/// `GET /eth/v1/beacon/states/head/validators/{pubkey}`, known for the fixture validator only.
async fn validator_handler(Path(pubkey): Path<String>) -> Result<Json<Value>, StatusCode> {
    if pubkey != validator_pubkey().to_string() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(
        json!({ "data": { "index": VALIDATOR_INDEX.to_string() } }),
    ))
}

async fn execution_proofs_handler(
    State(node): State<Arc<MockBeaconNode>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    assert_eq!(
        headers.get("content-type").unwrap(),
        "application/octet-stream"
    );
    for envelope in SignedExecutionProofEnvelopes::from_ssz_bytes(&body)
        .unwrap()
        .iter()
    {
        node.envelopes.send(envelope.clone()).unwrap();
    }
    StatusCode::OK
}

async fn start_mock_beacon_node(
    fixture: Fixture,
    ambiguous_block: bool,
    mismatched_slot: bool,
    block_from_event: bool,
) -> (
    Url,
    Arc<MockBeaconNode>,
    mpsc::UnboundedReceiver<SignedExecutionProofEnvelope>,
) {
    let (envelopes_tx, envelopes_rx) = mpsc::unbounded_channel();
    let node = Arc::new(MockBeaconNode {
        fixture,
        ambiguous_block,
        mismatched_slot,
        block_from_event,
        envelopes: envelopes_tx,
        events_opened: Notify::new(),
    });
    let app = axum::Router::new()
        .route(
            "/eth/v1/beacon/genesis",
            axum::routing::get(genesis_handler),
        )
        .route(
            "/eth/v1/beacon/headers",
            axum::routing::get(headers_handler),
        )
        .route("/eth/v1/events", axum::routing::get(events_handler))
        .route(
            "/eth/v1/beacon/states/head/validators/{pubkey}",
            axum::routing::get(validator_handler),
        )
        .route(
            "/eth/v2/beacon/blocks/{root}",
            axum::routing::get(block_handler),
        )
        .route("/eth/v1/config/spec", axum::routing::get(spec_handler))
        .route(
            "/eth/v1/beacon/execution_proofs",
            axum::routing::post(execution_proofs_handler),
        )
        .with_state(node.clone());
    (serve(app).await, node, envelopes_rx)
}

/// The chain spec of the mock beacon node. Every fork is at genesis, with the fixture fork
/// version under Gloas and the fixture chain id.
fn mock_spec(chain_id: u64) -> ChainSpec {
    let mut spec = ChainSpec::mainnet();
    spec.deposit_chain_id = chain_id;
    spec.altair_fork_epoch = Some(Epoch::new(0));
    spec.bellatrix_fork_epoch = Some(Epoch::new(0));
    spec.capella_fork_epoch = Some(Epoch::new(0));
    spec.deneb_fork_epoch = Some(Epoch::new(0));
    spec.electra_fork_epoch = Some(Epoch::new(0));
    spec.fulu_fork_epoch = Some(Epoch::new(0));
    spec.gloas_fork_epoch = Some(Epoch::new(0));
    spec.gloas_fork_version = FORK_VERSION;
    spec
}

/// The public key of the validator that signs the proofs.
fn validator_pubkey() -> PublicKey {
    Keystore::from_json_file(VOTING_KEYSTORE_PATH)
        .unwrap()
        .public_key()
        .unwrap()
}

async fn serve(app: axum::Router) -> Url {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://127.0.0.1:{port}").parse().unwrap()
}

#[derive(Default)]
struct Behavior {
    payload_invalid: bool,
    witness_unsupported: bool,
    proof_timeout: bool,
    proof_failure: bool,
    ambiguous_block: bool,
    mismatched_slot: bool,
    block_from_event: bool,
    beacon_node_unreachable: bool,
}

struct TestHarness {
    fixture: Fixture,
    zkboost_endpoint: Url,
    el: Arc<MockElState>,
    envelopes: mpsc::UnboundedReceiver<SignedExecutionProofEnvelope>,
    jwt_secret: JwtSecret,
    proof_types: Vec<ProofType>,
    shutdown: tokio_util::sync::CancellationToken,
}

impl TestHarness {
    async fn new(behavior: Behavior) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .try_init();
        let payload_status = if behavior.payload_invalid {
            "INVALID"
        } else {
            "VALID"
        };
        let (el_endpoint, el) = start_mock_el(
            Fixture::load(),
            payload_status,
            !behavior.witness_unsupported,
        )
        .await;
        let (beacon_endpoint, beacon_node, envelopes) = start_mock_beacon_node(
            Fixture::load(),
            behavior.ambiguous_block,
            behavior.mismatched_slot,
            behavior.block_from_event,
        )
        .await;
        let proof_timeout_secs = if behavior.proof_timeout { 1 } else { 12 };
        let proof_types = vec![ProofType::RethOpenVM];
        let config = Config {
            port: 0,
            el_engine_endpoint: el_endpoint,
            cl_beacon_endpoint: if behavior.beacon_node_unreachable {
                // Nothing listens on port 1, so every read of the beacon node fails.
                "http://127.0.0.1:1/".parse().unwrap()
            } else {
                beacon_endpoint
            },
            validator_keystore_path: VOTING_KEYSTORE_PATH.into(),
            validator_keystore_password_path: VOTING_KEYSTORE_PASSWORD_PATH.into(),
            dashboard: DashboardConfig::default(),
            zkvm: proof_types
                .iter()
                .map(|&proof_type| zkVMConfig::Mock {
                    proof_type,
                    proof_timeout_secs,
                    mock_proving_time: MockProvingTime::Constant { ms: 3000 },
                    mock_failure: behavior.proof_failure,
                })
                .collect(),
        };
        let metrics = PrometheusBuilder::new().build_recorder().handle();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let server = zkBoostServer::new(config, metrics).await.unwrap();
        let (addr, _) = server.run(shutdown.clone()).await.unwrap();
        if !behavior.beacon_node_unreachable {
            tokio::time::timeout(
                Duration::from_secs(10),
                beacon_node.events_opened.notified(),
            )
            .await
            .expect("validator should be ready");
        }
        Self {
            fixture: Fixture::load(),
            zkboost_endpoint: format!("http://127.0.0.1:{}", addr.port()).parse().unwrap(),
            el,
            envelopes,
            jwt_secret: JwtSecret::random(),
            proof_types,
            shutdown,
        }
    }

    /// Sends a JSON-RPC request to zkboost with a JWT and returns the response body.
    async fn engine_call(&self, method: &str, params: Value) -> Value {
        let token = self
            .jwt_secret
            .encode(&Claims::with_current_timestamp())
            .unwrap();
        let response = reqwest::Client::new()
            .post(self.zkboost_endpoint.clone())
            .bearer_auth(token)
            .json(&json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }

    async fn new_payload(&self) -> Value {
        let params = serde_json::to_value(&self.fixture.params).unwrap();
        self.engine_call(NewPayloadParams::METHOD, params).await
    }

    /// Waits for one signed envelope of every configured proof type, verifies the signature as
    /// the beacon node does, and returns the envelopes.
    async fn assert_proofs_submitted(&mut self) -> Vec<SignedExecutionProofEnvelope> {
        let pubkey = validator_pubkey();
        let domain = execution_proof_domain(FORK_VERSION, GENESIS_VALIDATORS_ROOT);
        let mut remaining: HashSet<u8> = self
            .proof_types
            .iter()
            .map(|proof_type| proof_type.execution_proof_type())
            .collect();
        let mut envelopes = Vec::new();
        while !remaining.is_empty() {
            let envelope = tokio::time::timeout(Duration::from_secs(60), self.envelopes.recv())
                .await
                .expect("proof should be submitted")
                .unwrap();
            assert_eq!(envelope.message.beacon_block_root, BEACON_BLOCK_ROOT.0);
            assert!(
                remaining.remove(&envelope.message.proof_type),
                "{envelope:?}"
            );
            assert_eq!(&*envelope.message.proof_data, MOCK_PROOF);
            assert_eq!(envelope.validator_index, VALIDATOR_INDEX);
            let signing_root = envelope.message.signing_root(domain);
            let signature = Signature::deserialize(&envelope.signature).unwrap();
            assert!(signature.verify(
                &pubkey,
                lighthouse_bls::Hash256::from_slice(signing_root.as_slice())
            ));
            envelopes.push(envelope);
        }
        envelopes
    }

    async fn assert_no_proof_submitted(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_secs(6), self.envelopes.recv())
                .await
                .is_err(),
            "no proof should be submitted"
        );
    }

    fn el_calls(&self) -> Vec<ElCall> {
        self.el.calls.lock().unwrap().clone()
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// The stateless input fixture matches the guest output fixture. A verifier therefore expects the
/// public values from the request root, the chain id, and the Amsterdam schema id.
#[test]
fn test_fixture_expected_output() {
    let fixture = Fixture::load();
    let expected = StatelessValidationResult::from_ssz_bytes(&hex!(
        "8c3a890206a189727e151767653f846ccddbd269eb29fb0a2f97371f23a481c6016ecca8a6010000000115"
    ))
    .unwrap();
    assert_eq!(
        expected.new_payload_request_root,
        fixture.new_payload_request_root.0
    );
    assert!(expected.successful_validation);
    assert_eq!(expected.chain_id, fixture.chain_id);
    assert_eq!(expected.schema_id, ProtocolFork::Amsterdam.schema_id());
}

#[tokio::test]
async fn test_new_payload_proof_submitted() {
    {
        let mut harness = TestHarness::new(Behavior::default()).await;

        let response = harness.new_payload().await;
        assert_eq!(response["id"], 7);
        assert_eq!(
            response["result"],
            json!({
                "status": "VALID",
                "latestValidHash": harness.fixture.block_hash,
                "validationError": null,
            })
        );

        let calls = harness.el_calls();
        let new_payload = calls
            .iter()
            .find(|call| call.method == NewPayloadParams::WITH_WITNESS_METHOD)
            .expect("new payload forwarded with witness");
        assert!(
            new_payload
                .authorization
                .as_deref()
                .is_some_and(|value| value.starts_with("Bearer ")),
            "{new_payload:?}"
        );

        harness.assert_proofs_submitted().await;

        // The same payload sent again is submitted from the cache, faster than a new proof.
        let resubmitted = Instant::now();
        harness.new_payload().await;
        harness.assert_proofs_submitted().await;
        assert!(resubmitted.elapsed() < Duration::from_millis(3000));
    }
}

#[tokio::test]
async fn test_other_methods_forwarded() {
    let harness = TestHarness::new(Behavior::default()).await;

    let response = harness
        .engine_call("engine_forkchoiceUpdatedV4", json!([{}, null, null]))
        .await;
    assert_eq!(response["result"]["payloadStatus"]["status"], "VALID");
    assert_eq!(response["result"]["payloadId"], Value::Null);

    let calls = harness.el_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "engine_forkchoiceUpdatedV4");
    assert!(calls[0].authorization.is_some());
}

/// An EL without the witness method answers the plain method, and nothing is proven.
#[tokio::test]
async fn test_witness_method_unsupported_forwarded() {
    let mut harness = TestHarness::new(Behavior {
        witness_unsupported: true,
        ..Default::default()
    })
    .await;

    let response = harness.new_payload().await;
    assert_eq!(response["result"]["status"], "VALID");

    let methods: Vec<_> = harness
        .el_calls()
        .into_iter()
        .map(|call| call.method)
        .collect();
    assert_eq!(
        methods,
        [
            NewPayloadParams::WITH_WITNESS_METHOD,
            NewPayloadParams::METHOD
        ]
    );

    harness.assert_no_proof_submitted().await;
}

#[tokio::test]
async fn test_invalid_payload_not_proven() {
    let mut harness = TestHarness::new(Behavior {
        payload_invalid: true,
        ..Default::default()
    })
    .await;

    let response = harness.new_payload().await;
    assert_eq!(response["result"]["status"], "INVALID");
    assert!(response["result"].get("witness").is_none());

    harness.assert_no_proof_submitted().await;
}

#[tokio::test]
async fn test_proof_failure_not_submitted() {
    let mut harness = TestHarness::new(Behavior {
        proof_failure: true,
        ..Default::default()
    })
    .await;

    harness.new_payload().await;
    harness.assert_no_proof_submitted().await;
}

#[tokio::test]
async fn test_proof_timeout_not_submitted() {
    let mut harness = TestHarness::new(Behavior {
        proof_timeout: true,
        ..Default::default()
    })
    .await;

    harness.new_payload().await;
    harness.assert_no_proof_submitted().await;
}

/// Two children of the parent block carry the payload, so the submission stops at the equivocation.
#[tokio::test]
async fn test_ambiguous_block_not_submitted() {
    let mut harness = TestHarness::new(Behavior {
        ambiguous_block: true,
        ..Default::default()
    })
    .await;

    let response = harness.new_payload().await;
    assert_eq!(response["result"]["status"], "VALID");

    harness.assert_no_proof_submitted().await;
}

/// The beacon block header reports a different slot, so the submission stops at the slot check.
#[tokio::test]
async fn test_mismatched_slot_not_submitted() {
    let mut harness = TestHarness::new(Behavior {
        mismatched_slot: true,
        ..Default::default()
    })
    .await;

    let response = harness.new_payload().await;
    assert_eq!(response["result"]["status"], "VALID");

    harness.assert_no_proof_submitted().await;
}

/// The `execution_payload` event carries the beacon block root, so no child is listed.
#[tokio::test]
async fn test_proof_submitted_from_execution_payload_event() {
    let mut harness = TestHarness::new(Behavior {
        block_from_event: true,
        ..Default::default()
    })
    .await;

    let response = harness.new_payload().await;
    assert_eq!(response["result"]["status"], "VALID");

    harness.assert_proofs_submitted().await;
}

/// Two `execution_payload` events carry the payload. The proof is submitted under the first
/// announced beacon block root.
#[tokio::test]
async fn test_first_execution_payload_event_kept() {
    let mut harness = TestHarness::new(Behavior {
        block_from_event: true,
        ambiguous_block: true,
        ..Default::default()
    })
    .await;

    let response = harness.new_payload().await;
    assert_eq!(response["result"]["status"], "VALID");

    for envelope in harness.assert_proofs_submitted().await {
        assert_eq!(envelope.message.beacon_block_root, BEACON_BLOCK_ROOT.0);
    }
}

/// The beacon node is unreachable, so the validator is never ready. The payload is forwarded
/// unchanged and nothing is proven.
#[tokio::test]
async fn test_payload_forwarded_before_validator_ready() {
    let mut harness = TestHarness::new(Behavior {
        beacon_node_unreachable: true,
        ..Default::default()
    })
    .await;

    let response = harness.new_payload().await;
    assert_eq!(response["result"]["status"], "VALID");

    let methods: Vec<_> = harness
        .el_calls()
        .into_iter()
        .map(|call| call.method)
        .collect();
    assert_eq!(methods, [NewPayloadParams::METHOD]);

    harness.assert_no_proof_submitted().await;
}

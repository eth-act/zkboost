//! The fake EL and the fake CL, with the payload of the fixture.

use std::{
    convert::Infallible,
    fs,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use alloy_primitives::{B256, b256, hex};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::sse::{Event, Sse},
    routing::{get, post},
};
use lighthouse_types::{
    BlobParameters, BlobSchedule, ChainSpec, Config as SpecConfig, Epoch, MainnetEthSpec,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::{Stream, StreamExt};

use crate::harness::{NEW_PAYLOAD, NEW_PAYLOAD_WITH_WITNESS, Scenario};

pub(crate) const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../server/tests/fixture");

/// The payload of block 93354 of glamsterdam-devnet-8.
const BLOCK_HASH: B256 =
    b256!("0x85548de50b6513bd3bfff100f9f2ad5316087c8ab73de9424d31b9b703b6a424");

const PARENT_BEACON_BLOCK_ROOT: B256 =
    b256!("0x1996b0d7d012895c2e44c72ea4171fe133ab9b175a67afddb11b2d46a6c62ab4");

pub(crate) const BLOCK_NUMBER: u64 = 93354;

const SLOT: u64 = 95877;

pub(crate) const TIMESTAMP: u64 = 1787772924;

pub(crate) const GAS_USED: u64 = 102412080;

const CHAIN_ID: u64 = 7091047534;

/// The beacon block of the payload, and another child of its parent.
const BEACON_BLOCK_ROOT: B256 = B256::repeat_byte(0xbb);

const OTHER_BLOCK_ROOT: B256 = B256::repeat_byte(0xcc);

/// The validator of the keystore.
const VALIDATOR_INDEX: u64 = 256;

/// Marks a request that the test sends to the EL itself, which the EL does not count.
pub(crate) const DIRECT: &str = "x-direct";

pub(crate) static PARAMS: LazyLock<Value> = LazyLock::new(|| {
    let params = fs::read(format!("{FIXTURE}/new_payload_params_amsterdam.json")).unwrap();
    serde_json::from_slice(&params).unwrap()
});

pub(crate) static WITNESS: LazyLock<Vec<u8>> =
    LazyLock::new(|| fs::read(format!("{FIXTURE}/witness_amsterdam.rlp")).unwrap());

static VALIDATOR_PUBKEY: LazyLock<String> = LazyLock::new(|| {
    let keystore = fs::read(format!("{FIXTURE}/voting-keystore.json")).unwrap();
    let keystore: Value = serde_json::from_slice(&keystore).unwrap();
    format!("0x{}", keystore["pubkey"].as_str().unwrap())
});

#[derive(Deserialize)]
struct HeadersQuery {
    slot: u64,
    parent_root: B256,
}

pub(crate) struct Fakes {
    scenario: Scenario,
    headers_error: AtomicBool,
    events_error: AtomicBool,
    /// The methods of the calls with a JWT that the EL got from zkboost.
    pub(crate) el_calls: Mutex<Vec<String>>,
    spec_invalid: AtomicBool,
    /// The lists of the children of a beacon block that the CL served. Only zkboost reads them.
    pub(crate) child_lists: AtomicUsize,
}

impl Fakes {
    pub(crate) fn new(scenario: Scenario) -> Arc<Self> {
        Arc::new(Self {
            scenario,
            headers_error: scenario.headers_error_once.into(),
            events_error: scenario.events_error_once.into(),
            el_calls: Mutex::default(),
            spec_invalid: scenario.spec_invalid_once.into(),
            child_lists: AtomicUsize::new(0),
        })
    }

    pub(crate) fn el(self: Arc<Self>) -> Router {
        Router::new()
            .route("/", post(Self::engine))
            .layer(DefaultBodyLimit::disable())
            .with_state(self)
    }

    pub(crate) fn cl(self: Arc<Self>) -> Router {
        let genesis = json!({ "data": { "genesis_validators_root": B256::repeat_byte(0x99) } });
        Router::new()
            .route("/eth/v1/beacon/genesis", get(Json(genesis)))
            .route("/eth/v1/config/spec", get(Self::spec))
            .route(
                "/eth/v1/beacon/states/head/validators/{id}",
                get(Self::validator),
            )
            .route("/eth/v1/beacon/headers", get(Self::headers))
            .route("/eth/v2/beacon/blocks/{root}", get(Self::block))
            .route("/eth/v1/events", get(Self::events))
            .with_state(self)
    }

    /// Like an EL, it rejects a call without a JWT before it reads it. Like geth, it sends the
    /// witness of a payload with the first response only, unless the scenario sends it every time.
    async fn engine(
        State(fakes): State<Arc<Self>>,
        headers: HeaderMap,
        body: String,
    ) -> Result<Json<Value>, StatusCode> {
        if headers.get(AUTHORIZATION).is_none() {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let request: Value = serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        let mut method_call_count = 0;
        if headers.get(DIRECT).is_none() {
            let mut el_calls = fakes.el_calls.lock().unwrap();
            el_calls.push(method.clone());
            method_call_count = el_calls.iter().filter(|call| **call == method).count();
        }
        let scenario = &fakes.scenario;
        let mut status = json!({ "status": scenario.payload_status, "latestValidHash": BLOCK_HASH, "validationError": null });
        let result = match method.as_str() {
            NEW_PAYLOAD_WITH_WITNESS if scenario.witness_unsupported => {
                let error = json!({ "code": -32601, "message": "Method not found" });
                return Ok(Json(
                    json!({ "jsonrpc": "2.0", "id": request["id"], "error": error }),
                ));
            }
            NEW_PAYLOAD_WITH_WITNESS => {
                if scenario.payload_status == "VALID"
                    && (method_call_count == 1 || scenario.witness_every_call)
                {
                    status["witness"] = json!(hex::encode_prefixed(&*WITNESS));
                }
                status
            }
            NEW_PAYLOAD => status,
            "engine_forkchoiceUpdatedV4" => json!({ "payloadStatus": status, "payloadId": null }),
            _ => Value::Null,
        };
        Ok(Json(
            json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }),
        ))
    }

    /// The mainnet spec with every fork at genesis, and BPO2 in one blob schedule entry.
    async fn spec(State(fakes): State<Arc<Self>>) -> Json<Value> {
        let mut spec = ChainSpec::mainnet();
        spec.deposit_chain_id = CHAIN_ID;
        spec.gloas_fork_version = [0x10, 0x00, 0x00, 0x38];
        for fork_epoch in [
            &mut spec.altair_fork_epoch,
            &mut spec.bellatrix_fork_epoch,
            &mut spec.capella_fork_epoch,
            &mut spec.deneb_fork_epoch,
            &mut spec.electra_fork_epoch,
            &mut spec.fulu_fork_epoch,
            &mut spec.gloas_fork_epoch,
        ] {
            *fork_epoch = Some(Epoch::new(0));
        }
        let mut config = SpecConfig::from_chain_spec::<MainnetEthSpec>(&spec);
        config.blob_schedule = BlobSchedule::new(vec![BlobParameters {
            epoch: Epoch::new(0),
            max_blobs_per_block: 21,
        }]);
        let mut config = json!(config);
        if fakes.spec_invalid.swap(false, Ordering::SeqCst) {
            config["ATTESTATION_DUE_BPS"] = json!("10001");
        }
        Json(json!({ "data": config }))
    }

    /// Knows the validator of the keystore by its public key and by its index.
    async fn validator(Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
        if id != *VALIDATOR_PUBKEY && id != VALIDATOR_INDEX.to_string() {
            return Err(StatusCode::NOT_FOUND);
        }
        let validator = json!({ "pubkey": *VALIDATOR_PUBKEY });
        Ok(Json(
            json!({ "data": { "index": VALIDATOR_INDEX.to_string(), "validator": validator } }),
        ))
    }

    /// Lists the children of a parent block at a slot. The other child is at the next slot under
    /// `other_slot_bid`.
    async fn headers(
        State(fakes): State<Arc<Self>>,
        Query(query): Query<HeadersQuery>,
    ) -> Result<Json<Value>, StatusCode> {
        if fakes.headers_error.swap(false, Ordering::SeqCst) {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        fakes.child_lists.fetch_add(1, Ordering::SeqCst);
        let scenario = &fakes.scenario;
        let children = [
            (OTHER_BLOCK_ROOT, SLOT + u64::from(scenario.other_slot_bid)),
            (BEACON_BLOCK_ROOT, SLOT),
        ];
        let listed = query.parent_root == PARENT_BEACON_BLOCK_ROOT && !scenario.block_from_event;
        let at_slot = children
            .iter()
            .filter(|(_, slot)| listed && *slot == query.slot);
        let headers = Vec::from_iter(at_slot.map(|(root, _)| json!({ "root": root })));
        Ok(Json(json!({ "data": headers })))
    }

    async fn block(
        State(fakes): State<Arc<Self>>,
        Path(root): Path<B256>,
    ) -> Result<Json<Value>, StatusCode> {
        let scenario = &fakes.scenario;
        let block_hash = match root {
            BEACON_BLOCK_ROOT => BLOCK_HASH,
            OTHER_BLOCK_ROOT if scenario.ambiguous_block || scenario.other_slot_bid => BLOCK_HASH,
            OTHER_BLOCK_ROOT => B256::ZERO,
            _ => return Err(StatusCode::NOT_FOUND),
        };
        let bid = json!({ "message": { "block_hash": block_hash } });
        Ok(Json(
            json!({ "data": { "message": { "body": { "signed_execution_payload_bid": bid } } } }),
        ))
    }

    /// Announces the beacon block under `block_from_event`, and then the other block under
    /// `ambiguous_block`. The stream stays open.
    async fn events(
        State(fakes): State<Arc<Self>>,
    ) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
        if fakes.events_error.swap(false, Ordering::SeqCst) {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let scenario = &fakes.scenario;
        let block_roots = [
            scenario.block_from_event.then_some(BEACON_BLOCK_ROOT),
            (scenario.block_from_event && scenario.ambiguous_block).then_some(OTHER_BLOCK_ROOT),
        ];
        let events = block_roots.into_iter().flatten().map(|block_root| {
            let data = json!({ "slot": SLOT.to_string(), "block_hash": BLOCK_HASH, "block_root": block_root });
            Ok(Event::default().event("execution_payload").data(data.to_string()))
        });
        Ok(Sse::new(
            tokio_stream::iter(events).chain(tokio_stream::pending()),
        ))
    }
}

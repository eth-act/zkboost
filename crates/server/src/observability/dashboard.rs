//! The live dashboard, an add-on that shows the recent payloads and their proofs in real time.
//! The dispatcher reports to it where it records the metrics. Every report updates the snapshot
//! of `/dashboard/state` and goes to `/dashboard/events` as a server-sent event. Every time is
//! in seconds since the start of the slot of the payload.

use std::{
    collections::HashMap,
    convert::Infallible,
    mem,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::B256;
use axum::{
    Json, Router,
    extract::State,
    response::{
        Html,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use lru::LruCache;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};

use crate::{
    engine::payload::PayloadHeader, observability::metrics::ProveStatus,
    proof::proof_type::ProofType,
};

pub(crate) struct Dashboard {
    proof_types: Vec<ProofType>,
    blocks: Mutex<LruCache<B256, Block>>,
    event_tx: broadcast::Sender<DashboardEvent>,
}

#[derive(Clone)]
struct DashboardEvent {
    name: &'static str,
    data: Value,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Block {
    block_number: u64,
    block_hash: B256,
    block_timestamp: u64,
    gas_used: u64,
    gas_limit: u64,
    witness_started_s: f64,
    witness_ended_s: f64,
    witness_size: usize,
    witness_success: bool,
    proofs: HashMap<ProofType, Proof>,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Proof {
    started_s: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proof_size: Option<usize>,
}

/// Returns the seconds from the Unix `timestamp` until `time`.
fn seconds_since(timestamp: u64, time: SystemTime) -> f64 {
    let time = time
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the Unix epoch");
    time.as_secs_f64() - timestamp as f64
}

impl Dashboard {
    pub(crate) fn new(mut proof_types: Vec<ProofType>, retention: NonZeroUsize) -> Self {
        proof_types.sort();
        Self {
            proof_types,
            blocks: Mutex::new(LruCache::new(retention)),
            event_tx: broadcast::channel(128).0,
        }
    }

    pub(crate) fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route(
                "/dashboard",
                get(Html(include_str!("dashboard/index.html"))),
            )
            .route("/dashboard/state", get(Self::state))
            .route("/dashboard/events", get(Self::events))
            .with_state(self)
    }

    async fn state(State(dashboard): State<Arc<Self>>) -> Json<Value> {
        let blocks = dashboard.blocks.lock().unwrap();
        Json(json!({
            "buildVersion": env!("CARGO_PKG_VERSION"),
            "proofTypes": dashboard.proof_types,
            "historicalBlocks": Vec::from_iter(blocks.iter().map(|(_, block)| block)),
            "retention": blocks.cap(),
        }))
    }

    /// Streams every event. A client that lags loses its stream, connects again, and reads a new
    /// snapshot.
    async fn events(
        State(dashboard): State<Arc<Self>>,
    ) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
        let events = BroadcastStream::new(dashboard.event_tx.subscribe())
            .map_while(Result::ok)
            .map(|event| {
                Ok(Event::default()
                    .event(event.name)
                    .data(event.data.to_string()))
            });
        Sse::new(events).keep_alive(KeepAlive::default())
    }

    fn send(&self, name: &'static str, data: Value) {
        let _ = self.event_tx.send(DashboardEvent { name, data });
    }

    /// Reports the witness fetch of a payload. The record of a successful fetch stays, because
    /// a payload imported again comes without a witness.
    pub(crate) fn witness_fetched(
        &self,
        header: &PayloadHeader,
        received: SystemTime,
        witness_fetch: Duration,
        witness_size: usize,
        success: bool,
    ) {
        let mut blocks = self.blocks.lock().unwrap();
        if (blocks.peek(&header.block_hash)).is_some_and(|block| block.witness_success) {
            return;
        }
        if !blocks.contains(&header.block_hash) {
            blocks.put(header.block_hash, Block::default());
        }
        let block = blocks
            .peek_mut(&header.block_hash)
            .expect("the block is present");
        *block = Block {
            block_number: header.block_number,
            block_hash: header.block_hash,
            block_timestamp: header.timestamp,
            gas_used: header.gas_used,
            gas_limit: header.gas_limit,
            witness_started_s: seconds_since(header.timestamp, received),
            witness_ended_s: seconds_since(header.timestamp, received + witness_fetch),
            witness_size,
            witness_success: success,
            proofs: mem::take(&mut block.proofs),
        };
        let mut start = json!(block);
        start["startedS"] = json!(block.witness_started_s);
        self.send("fetchWitnessStart", start);
        self.send(
            "fetchWitnessEnd",
            json!({
                "blockHash": block.block_hash,
                "endedS": block.witness_ended_s,
                "witnessSize": witness_size,
                "success": success,
            }),
        );
    }

    pub(crate) fn prove_started(&self, block_hash: B256, proof_type: ProofType) {
        let mut blocks = self.blocks.lock().unwrap();
        let Some(block) = blocks.peek_mut(&block_hash) else {
            return;
        };
        let started_s = seconds_since(block.block_timestamp, SystemTime::now());
        block.proofs.entry(proof_type).or_default().started_s = started_s;
        self.send(
            "proveStart",
            json!({ "blockHash": block_hash, "proofType": proof_type, "startedS": started_s }),
        );
    }

    pub(crate) fn prove_ended(
        &self,
        block_hash: B256,
        proof_type: ProofType,
        status: ProveStatus,
        error: Option<String>,
        proof_size: Option<usize>,
    ) {
        let mut blocks = self.blocks.lock().unwrap();
        let Some(block) = blocks.peek_mut(&block_hash) else {
            return;
        };
        let ended_s = seconds_since(block.block_timestamp, SystemTime::now());
        let proof = block.proofs.entry(proof_type).or_default();
        proof.ended_s = Some(ended_s);
        proof.result = Some(status.into());
        proof.error = error;
        proof.proof_size = proof_size;
        let mut end = json!(proof);
        end["blockHash"] = json!(block_hash);
        end["proofType"] = json!(proof_type);
        self.send("proveEnd", end);
    }
}

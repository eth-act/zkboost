//! Dashboard service that tracks block proving activity and broadcasts SSE events for the live
//! dashboard UI.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use lru::LruCache;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::info;
use zkboost_types::{Hash256, MainnetEthSpec, NewPayloadRequest, ProofType};

use crate::proof::worker::ProofResult as WorkerProofResult;

/// Internal dashboard state behind `RwLock`, not directly serialized.
#[derive(Debug)]
pub(crate) struct DashboardState {
    build_version: String,
    proof_types: Vec<ProofType>,
    historical_blocks: LruCache<Hash256, HistoricalBlock>,
}

impl DashboardState {
    pub(crate) fn new(proof_types: impl IntoIterator<Item = ProofType>, retention: usize) -> Self {
        let mut proof_types = proof_types.into_iter().collect::<Vec<_>>();
        proof_types.sort();
        Self {
            build_version: env!("CARGO_PKG_VERSION").to_owned(),
            proof_types,
            historical_blocks: LruCache::new(
                NonZeroUsize::new(retention).expect("retention must be non-zero"),
            ),
        }
    }

    pub(crate) fn to_response(&self) -> DashboardStateResponse {
        DashboardStateResponse {
            build_version: self.build_version.clone(),
            proof_types: self.proof_types.clone(),
            historical_blocks: self
                .historical_blocks
                .iter()
                .map(|(_, block)| block)
                .cloned()
                .collect(),
            retention: self.historical_blocks.cap().get(),
        }
    }

    fn insert_block(&mut self, hash: Hash256, record: HistoricalBlock) {
        if self.historical_blocks.contains(&hash) {
            return;
        }
        self.historical_blocks.push(hash, record);
    }

    fn get_block_mut(&mut self, hash: &Hash256) -> Option<&mut HistoricalBlock> {
        self.historical_blocks.peek_mut(hash)
    }
}

/// JSON response for the dashboard state endpoint.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DashboardStateResponse {
    /// Server build version.
    pub(crate) build_version: String,
    /// All configured proof types.
    pub(crate) proof_types: Vec<ProofType>,
    /// Historical block records, newest first.
    pub(crate) historical_blocks: Vec<HistoricalBlock>,
    /// Maximum number of blocks retained in history.
    pub(crate) retention: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProofResult {
    Success,
    Error,
    Timeout,
}

/// Record of a block's proving pipeline state.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoricalBlock {
    /// Block number.
    pub(crate) block_number: u64,
    /// Hex-encoded block hash.
    pub(crate) block_hash: Hash256,
    /// Block timestamp (unix seconds).
    pub(crate) block_timestamp: u64,
    /// Gas used by the block.
    pub(crate) gas_used: u64,
    /// Seconds since block timestamp when witness fetch started.
    pub(crate) witness_started_s: Option<f64>,
    /// Seconds since block timestamp when witness fetch ended.
    pub(crate) witness_ended_s: Option<f64>,
    /// Witness size in bytes.
    pub(crate) witness_size: Option<u64>,
    /// Whether the witness was fetched successfully.
    pub(crate) witness_success: bool,
    /// Proof records keyed by proof type.
    pub(crate) proofs: HashMap<ProofType, HistoricalProof>,
}

/// Record of a single proof attempt. Created at prove start with optional fields filled in at prove
/// end.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoricalProof {
    /// Seconds since block timestamp when the proof was requested. None if unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requested_s: Option<f64>,
    /// Seconds since block timestamp when proving started. None before proving begins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) started_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<ProofResult>,
    /// Error message on failure. None while proving, on success, or on timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// Seconds since block timestamp when proving ended. None while proving.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ended_s: Option<f64>,
    /// Proof size in bytes. None while proving or on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proof_size: Option<u64>,
}

/// Messages consumed by the dashboard service event loop.
#[derive(Debug)]
pub(crate) enum DashboardMessage {
    /// A new proof request was submitted for a block.
    RequestProof {
        block_number: u64,
        block_hash: Hash256,
        block_timestamp: u64,
        gas_used: u64,
        proof_types: Vec<ProofType>,
        timestamp_secs: f64,
    },
    /// Witness fetch started for a block.
    FetchWitnessStart {
        block_hash: Hash256,
        timestamp_secs: f64,
    },
    /// Witness fetch completed for a block (success or timeout).
    FetchWitnessEnd {
        block_hash: Hash256,
        witness_size: usize,
        success: bool,
        timestamp_secs: f64,
    },
    /// Proving started for a block and proof type.
    ProveStart {
        block_hash: Hash256,
        proof_type: ProofType,
        timestamp_secs: f64,
    },
    /// Proving finished for a block and proof type.
    ProveEnd {
        block_hash: Hash256,
        proof_type: ProofType,
        result: ProofResult,
        error: Option<String>,
        proof_size: Option<u64>,
        timestamp_secs: f64,
    },
}

impl DashboardMessage {
    pub(crate) fn request_proof(
        request: &NewPayloadRequest<MainnetEthSpec>,
        proof_types: &HashSet<ProofType>,
    ) -> Self {
        let mut proof_types: Vec<_> = proof_types.iter().copied().collect();
        proof_types.sort();
        Self::RequestProof {
            block_number: request.block_number(),
            block_hash: request.block_hash(),
            block_timestamp: request.timestamp(),
            gas_used: request.gas_used(),
            proof_types,
            timestamp_secs: now_secs(),
        }
    }

    pub(crate) fn fetch_witness_start(block_hash: Hash256) -> Self {
        Self::FetchWitnessStart {
            block_hash,
            timestamp_secs: now_secs(),
        }
    }

    pub(crate) fn fetch_witness_end(
        block_hash: Hash256,
        witness_size: usize,
        success: bool,
    ) -> Self {
        Self::FetchWitnessEnd {
            block_hash,
            witness_size,
            success,
            timestamp_secs: now_secs(),
        }
    }

    pub(crate) fn prove_start(block_hash: Hash256, proof_type: ProofType) -> Self {
        Self::ProveStart {
            block_hash,
            proof_type,
            timestamp_secs: now_secs(),
        }
    }

    pub(crate) fn prove_end(
        block_hash: Hash256,
        proof_type: ProofType,
        proof_result: &WorkerProofResult,
    ) -> Self {
        let (result, error, proof_size) = match proof_result {
            WorkerProofResult::Ok(bytes) => (ProofResult::Success, None, Some(bytes.len() as u64)),
            WorkerProofResult::Err(msg) => (ProofResult::Error, Some(msg.clone()), None),
            WorkerProofResult::Timeout => (ProofResult::Timeout, None, None),
        };
        Self::ProveEnd {
            block_hash,
            proof_type,
            result,
            error,
            proof_size,
            timestamp_secs: now_secs(),
        }
    }
}

/// SSE event broadcast to dashboard clients.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum DashboardEvent {
    /// A new proof request was submitted.
    #[serde(rename_all = "camelCase")]
    RequestProof {
        block_number: u64,
        block_hash: Hash256,
        block_timestamp: u64,
        gas_used: u64,
        started_s: f64,
        proof_types: Vec<ProofType>,
    },
    /// Witness fetch started.
    #[serde(rename_all = "camelCase")]
    FetchWitnessStart { block_hash: Hash256, started_s: f64 },
    /// Witness fetch completed (success or timeout).
    #[serde(rename_all = "camelCase")]
    FetchWitnessEnd {
        block_hash: Hash256,
        ended_s: f64,
        witness_size: u64,
        success: bool,
    },
    /// Proving started for a specific proof type.
    #[serde(rename_all = "camelCase")]
    ProveStart {
        block_hash: Hash256,
        proof_type: ProofType,
        started_s: f64,
    },
    /// Proving finished for a specific proof type.
    #[serde(rename_all = "camelCase")]
    ProveEnd {
        block_hash: Hash256,
        proof_type: ProofType,
        ended_s: f64,
        result: ProofResult,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        proof_size: Option<u64>,
    },
}

impl DashboardEvent {
    /// Returns the SSE event name and JSON-serialized data for this event.
    pub(crate) fn to_parts(&self) -> (&'static str, String) {
        let event_name = match self {
            Self::RequestProof { .. } => "requestProof",
            Self::FetchWitnessStart { .. } => "fetchWitnessStart",
            Self::FetchWitnessEnd { .. } => "fetchWitnessEnd",
            Self::ProveStart { .. } => "proveStart",
            Self::ProveEnd { .. } => "proveEnd",
        };
        let data = serde_json::to_string(self).expect("DashboardEvent serialization is infallible");
        (event_name, data)
    }
}

/// Service that maintains dashboard state and broadcasts SSE events for live display.
pub(crate) struct DashboardService {
    state: Arc<RwLock<DashboardState>>,
    event_tx: broadcast::Sender<DashboardEvent>,
}

impl DashboardService {
    /// Creates a new dashboard service.
    pub(crate) fn new(
        state: Arc<RwLock<DashboardState>>,
        event_tx: broadcast::Sender<DashboardEvent>,
    ) -> Self {
        Self { state, event_tx }
    }

    /// Runs the dashboard service event loop until shutdown is signalled.
    pub(crate) async fn run(
        mut self,
        shutdown: CancellationToken,
        mut rx: mpsc::Receiver<DashboardMessage>,
    ) {
        loop {
            tokio::select! {
                biased;

                _ = shutdown.cancelled() => {
                    info!("dashboard service shutting down");
                    break;
                }

                Some(msg) = rx.recv() => self.handle_message(msg).await,

                else => break,
            }
        }
    }

    async fn handle_message(&mut self, msg: DashboardMessage) {
        match msg {
            DashboardMessage::RequestProof {
                block_number,
                block_hash,
                block_timestamp,
                gas_used,
                proof_types,
                timestamp_secs,
            } => {
                let started_s = timestamp_secs - block_timestamp as f64;
                let mut state = self.state.write().await;
                state.insert_block(
                    block_hash,
                    HistoricalBlock {
                        block_number,
                        block_hash,
                        block_timestamp,
                        gas_used,
                        ..Default::default()
                    },
                );
                if let Some(block) = state.get_block_mut(&block_hash) {
                    for &proof_type in &proof_types {
                        block.proofs.entry(proof_type).or_insert(HistoricalProof {
                            requested_s: Some(started_s),
                            ..Default::default()
                        });
                    }
                }
                drop(state);

                let _ = self.event_tx.send(DashboardEvent::RequestProof {
                    block_number,
                    block_hash,
                    block_timestamp,
                    gas_used,
                    started_s,
                    proof_types,
                });
            }
            DashboardMessage::FetchWitnessStart {
                block_hash,
                timestamp_secs,
            } => {
                let mut state = self.state.write().await;
                let Some(block) = state.get_block_mut(&block_hash) else {
                    return;
                };
                let started_s = timestamp_secs - block.block_timestamp as f64;
                block.witness_started_s = Some(started_s);
                drop(state);

                let _ = self.event_tx.send(DashboardEvent::FetchWitnessStart {
                    block_hash,
                    started_s,
                });
            }
            DashboardMessage::FetchWitnessEnd {
                block_hash,
                witness_size,
                success,
                timestamp_secs,
            } => {
                let mut state = self.state.write().await;
                let Some(block) = state.get_block_mut(&block_hash) else {
                    return;
                };
                let ended_s = timestamp_secs - block.block_timestamp as f64;
                block.witness_ended_s = Some(ended_s);
                block.witness_size = Some(witness_size as u64);
                block.witness_success = success;
                drop(state);

                let _ = self.event_tx.send(DashboardEvent::FetchWitnessEnd {
                    block_hash,
                    ended_s,
                    witness_size: witness_size as u64,
                    success,
                });
            }
            DashboardMessage::ProveStart {
                block_hash,
                proof_type,
                timestamp_secs,
            } => {
                let mut state = self.state.write().await;
                let Some(block) = state.get_block_mut(&block_hash) else {
                    return;
                };
                let started_s = timestamp_secs - block.block_timestamp as f64;
                if let Some(record) = block.proofs.get_mut(&proof_type) {
                    record.started_s = Some(started_s);
                };
                drop(state);

                let _ = self.event_tx.send(DashboardEvent::ProveStart {
                    block_hash,
                    proof_type,
                    started_s,
                });
            }
            DashboardMessage::ProveEnd {
                block_hash,
                proof_type,
                result,
                error,
                proof_size,
                timestamp_secs,
            } => {
                let mut state = self.state.write().await;
                let Some(block) = state.get_block_mut(&block_hash) else {
                    return;
                };
                let ended_s = timestamp_secs - block.block_timestamp as f64;
                if let Some(record) = block.proofs.get_mut(&proof_type) {
                    record.result = Some(result);
                    record.error = error.clone();
                    record.ended_s = Some(ended_s);
                    record.proof_size = proof_size;
                }
                drop(state);

                let _ = self.event_tx.send(DashboardEvent::ProveEnd {
                    block_hash,
                    proof_type,
                    ended_s,
                    result,
                    error,
                    proof_size,
                });
            }
        }
    }
}

/// Returns the current wall-clock time as seconds since the Unix epoch.
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

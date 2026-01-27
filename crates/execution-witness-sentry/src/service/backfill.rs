//! # Backfill Service
//!
//! This module provides [`BackfillService`], which monitors zkVM-enabled CL clients and triggers
//! proof backfill when they fall behind the source CL.
//!
//! ## Purpose
//!
//! The backfill service ensures that zkVM-enabled CL clients stay synchronized with the source CL
//! by periodically checking for gaps and requesting missing proofs.

use std::{sync::Arc, time::Duration};

use alloy_primitives::map::HashMap;
use lru::LruCache;
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    BlockStorage, ClClient, ElBlockWitness,
    rpc::Hash256,
    service::{
        Target, el_data::ElDataServiceMessage, is_el_data_available, proof::ProofServiceMessage,
    },
};

/// Status of a zkVM-enabled CL client relative to the source CL.
///
/// Used to determine which clients are behind and need backfilling.
#[derive(Debug, Clone)]
#[allow(non_camel_case_types)]
pub struct zkVMEnabledClStatus {
    /// Human-readable name of the zkVM-enabled CL client.
    pub name: String,
    /// Current head slot of this zkVM-enabled CL client.
    pub head_slot: u64,
    /// Slot difference from the source CL (negative means behind).
    ///
    /// Calculated as `zkvm_head - source_head`, so a value of -5 means
    /// the zkVM client is 5 slots behind the source.
    pub gap: i64,
}

/// Monitors zkVM-enabled CL clients and triggers proof backfill when they fall behind.
///
/// The backfill service periodically compares the head slot of each zkVM-enabled CL client
/// against the source CL. When a client falls more than 5 slots behind, it initiates
/// backfill by requesting proofs for missing slots.
///
/// ## Data Flow
///
/// When backfilling, for each missing slot:
/// 1. Fetches block info from the source CL
/// 2. Skip if that slot is missing
/// 3. If EL data is not cached, sends [`ElDataServiceMessage::FetchData`]
/// 4. Sends [`ProofServiceMessage::RequestProof`] targeting the specific behind client
pub struct BackfillService {
    /// Reference CL client used to determine the canonical head.
    source_cl_client: Arc<ClClient>,
    /// CL clients that require zkVM proofs for block validation.
    zkvm_enabled_cl_clients: HashMap<String, Arc<ClClient>>,
    /// Shared LRU cache of execution block data.
    el_data_cache: Arc<Mutex<LruCache<Hash256, ElBlockWitness>>>,
    /// Optional persistent storage for proofs.
    storage: Option<Arc<Mutex<BlockStorage>>>,
    /// Channel for sending proof generation requests to [`ProofService`].
    proof_tx: mpsc::Sender<ProofServiceMessage>,
    /// Channel for requesting EL block data from [`ElDataService`].
    el_data_tx: mpsc::Sender<ElDataServiceMessage>,
    /// Polling interval in milliseconds between status checks.
    interval_ms: u64,
}

impl BackfillService {
    /// Creates a new backfill service.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_cl_client: Arc<ClClient>,
        zkvm_enabled_cl_clients: Vec<Arc<ClClient>>,
        el_data_cache: Arc<Mutex<LruCache<Hash256, ElBlockWitness>>>,
        storage: Option<Arc<Mutex<BlockStorage>>>,
        proof_tx: mpsc::Sender<ProofServiceMessage>,
        el_data_tx: mpsc::Sender<ElDataServiceMessage>,
        interval_ms: u64,
    ) -> Self {
        let zkvm_enabled_cl_clients = zkvm_enabled_cl_clients
            .into_iter()
            .map(|client| (client.name().to_string(), client))
            .collect();
        Self {
            source_cl_client,
            zkvm_enabled_cl_clients,
            proof_tx,
            el_data_tx,
            el_data_cache,
            storage,
            interval_ms,
        }
    }

    /// Spawns the backfill service as a background task.
    ///
    /// The service runs until the shutdown token is cancelled.
    pub fn spawn(self: Arc<Self>, shutdown_token: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(self.run(shutdown_token))
    }

    /// Main event loop that periodically checks client statuses and triggers backfill.
    ///
    /// Uses a timer-based polling approach with configurable interval.
    async fn run(self: Arc<Self>, shutdown_token: CancellationToken) {
        let mut interval = tokio::time::interval(Duration::from_millis(self.interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        info!(interval_ms = self.interval_ms, "BackfillService started");

        loop {
            tokio::select! {
                biased;

                _ = shutdown_token.cancelled() => {
                    info!("BackfillService received shutdown signal");
                    break;
                }

                _ = interval.tick() => {
                    let statuses = self.get_statuses().await;

                    for status in &statuses {
                        if status.gap < -5 {
                            warn!(
                                name = %status.name,
                                head_slot = status.head_slot,
                                gap = status.gap,
                                "zkvm CL is behind, starting backfill"
                            );

                            if let Some(client) = self.zkvm_enabled_cl_clients.get(&status.name)

                            {
                                self.backfill_proofs(client, 20).await;
                            }
                        } else if status.gap < 0 {
                            debug!(
                                name = %status.name,
                                head_slot = status.head_slot,
                                gap = status.gap,
                                "zkvm CL slightly behind"
                            );
                        } else {
                            debug!(
                                name = %status.name,
                                head_slot = status.head_slot,
                                gap = status.gap,
                                "zkvm CL in sync"
                            );
                        }
                    }
                }
            }
        }

        info!("BackfillService stopped");
    }

    /// Queries all zkVM-enabled CL clients and computes their gap from the source CL.
    ///
    /// Returns an empty vector if the source CL head cannot be retrieved.
    /// Individual client failures are logged but don't prevent other clients from being checked.
    async fn get_statuses(&self) -> Vec<zkVMEnabledClStatus> {
        let source_head = match self.source_cl_client.get_head_slot().await {
            Ok(slot) => slot,
            Err(e) => {
                warn!(error = %e, "Failed to get source CL head");
                return vec![];
            }
        };

        let mut statuses = Vec::new();
        for client in self.zkvm_enabled_cl_clients.values() {
            match client.get_head_slot().await {
                Ok(head_slot) => {
                    let gap = head_slot as i64 - source_head as i64;
                    statuses.push(zkVMEnabledClStatus {
                        name: client.name().to_string(),
                        head_slot,
                        gap,
                    });
                }
                Err(e) => {
                    warn!(name = %client.name(), error = %e, "Failed to get zkvm CL head");
                }
            }
        }

        statuses
    }

    /// Requests proofs for slots where the given zkVM client is behind.
    ///
    /// Iterates through slots from the client's current head up to `max_slots` ahead.
    ///
    /// For each slot, request EL data fetch if it is not available, then submits a proof request.
    async fn backfill_proofs(&self, zkvm_enabled_client: &ClClient, max_slots: u64) {
        let zkvm_head = match zkvm_enabled_client.get_head_slot().await {
            Ok(slot) => slot,
            Err(e) => {
                warn!(name = %zkvm_enabled_client.name(), error = %e, "Failed to get zkvm CL head for backfill");
                return;
            }
        };

        let source_head = match self.source_cl_client.get_head_slot().await {
            Ok(slot) => slot,
            Err(e) => {
                warn!(error = %e, "Failed to get source CL head for backfill");
                return;
            }
        };

        if zkvm_head >= source_head {
            return;
        }

        let gap = source_head - zkvm_head;
        let slots_to_check = gap.min(max_slots);

        info!(
            name = %zkvm_enabled_client.name(),
            zkvm_head = zkvm_head,
            source_head = source_head,
            gap = gap,
            checking = slots_to_check,
            "Backfilling proofs"
        );

        // TODO: Track proof that's already backfilled, if it still fall behind,
        //       it'd probably be other issues instead of missing proofs.

        for slot in (zkvm_head + 1)..=(zkvm_head + slots_to_check) {
            let block_info = match self.source_cl_client.get_block_info(slot).await {
                Ok(Some(info)) => info,
                Ok(None) => {
                    debug!(slot = slot, "Empty slot, skipping");
                    continue;
                }
                Err(e) => {
                    debug!(slot = slot, error = %e, "Failed to get block info");
                    continue;
                }
            };

            let Some(block_hash) = block_info.execution_block_hash else {
                debug!(slot = slot, "No execution payload, skipping");
                continue;
            };

            if !is_el_data_available(&self.el_data_cache, &self.storage, block_hash).await {
                debug!(slot = slot, block_hash = %block_hash, "EL data not ready for backfill, sending fetch and proof request");

                let msg = ElDataServiceMessage::FetchData { block_hash };
                if let Err(e) = self.el_data_tx.send(msg).await {
                    error!(error = %e, "Failed to send block fetch request for backfill");
                }
            }

            let msg = ProofServiceMessage::RequestProof {
                slot,
                block_root: block_info.block_root,
                execution_block_hash: block_hash,
                target_clients: [zkvm_enabled_client.name().to_string()]
                    .into_iter()
                    .collect(),
                target_proof_types: Target::All,
            };
            if self.proof_tx.send(msg).await.is_err() {
                error!("Failed to send backfill proof request");
            }
        }
    }
}

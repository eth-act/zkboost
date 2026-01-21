use std::{sync::Arc, time::Duration};

use lru::LruCache;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    BlockStorage, ClClient, ElBlockWitness,
    service::{
        Target, el_data::ElDataServiceMessage, is_el_data_ready, proof::ProofServiceMessage,
    },
};

#[derive(Debug, Clone)]
pub struct ZkvmClStatus {
    pub name: String,
    pub head_slot: u64,
    pub gap: i64,
}

pub struct BackfillService {
    source_cl_client: Arc<ClClient>,
    zkvm_enabled_cl_clients: Vec<Arc<ClClient>>,
    block_cache: Arc<Mutex<LruCache<String, ElBlockWitness>>>,
    storage: Option<Arc<Mutex<BlockStorage>>>,
    proof_tx: mpsc::Sender<ProofServiceMessage>,
    el_data_tx: mpsc::Sender<ElDataServiceMessage>,
    interval_ms: u64,
}

impl BackfillService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_cl_client: Arc<ClClient>,
        zkvm_enabled_cl_clients: Vec<Arc<ClClient>>,
        block_cache: Arc<Mutex<LruCache<String, ElBlockWitness>>>,
        storage: Option<Arc<Mutex<BlockStorage>>>,
        proof_tx: mpsc::Sender<ProofServiceMessage>,
        el_data_tx: mpsc::Sender<ElDataServiceMessage>,
        interval_ms: u64,
    ) -> Self {
        Self {
            source_cl_client,
            zkvm_enabled_cl_clients,
            proof_tx,
            el_data_tx,
            block_cache,
            storage,
            interval_ms,
        }
    }

    pub async fn run(self, shutdown_token: CancellationToken) {
        let mut monitor_interval = tokio::time::interval(Duration::from_millis(self.interval_ms));
        monitor_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        info!(interval_ms = self.interval_ms, "BackfillService started");

        loop {
            tokio::select! {
                biased;

                _ = shutdown_token.cancelled() => {
                    info!("BackfillService received shutdown signal");
                    break;
                }

                _ = monitor_interval.tick() => {
                    let statuses = self.monitor_zkvm_status().await;

                    for status in &statuses {
                        if status.gap < -5 {
                            warn!(
                                name = %status.name,
                                head_slot = status.head_slot,
                                gap = status.gap,
                                "zkvm CL is behind, starting backfill"
                            );

                            if let Some(client) = self.zkvm_enabled_cl_clients
                                .iter()
                                .find(|client| client.name() == status.name)
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

    async fn monitor_zkvm_status(&self) -> Vec<ZkvmClStatus> {
        let source_head = match self.source_cl_client.get_head_slot().await {
            Ok(slot) => slot,
            Err(e) => {
                warn!(error = %e, "Failed to get source CL head");
                return vec![];
            }
        };

        let mut statuses = Vec::new();
        for client in &self.zkvm_enabled_cl_clients {
            match client.get_head_slot().await {
                Ok(head_slot) => {
                    let gap = head_slot as i64 - source_head as i64;
                    statuses.push(ZkvmClStatus {
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

            if !is_el_data_ready(&self.block_cache, &self.storage, &block_hash).await {
                debug!(slot = slot, block_hash = %block_hash, "EL data not ready for backfill, sending fetch and proof request");

                if let Err(e) = self
                    .el_data_tx
                    .send(ElDataServiceMessage::FetchData {
                        block_hash: block_hash.clone(),
                    })
                    .await
                {
                    error!(error = %e, "Failed to send block fetch request for backfill");
                }
            }

            let message = ProofServiceMessage::RequestProof {
                cl_name: self.source_cl_client.name().to_string(),
                slot,
                block_root: block_info.block_root.clone(),
                execution_block_hash: block_hash.clone(),
                target_clients: [zkvm_enabled_client.name().to_string()]
                    .into_iter()
                    .collect(),
                target_proof_types: Target::All,
            };

            if self.proof_tx.send(message).await.is_err() {
                error!("Failed to send backfill proof request");
            }
        }
    }
}

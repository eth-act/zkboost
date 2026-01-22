//! # CL Event Service
//!
//! This module provides [`ClEventService`], which subscribes to CL head events via Server-Sent
//! Events (SSE) and triggers proof requests.
//!
//! ## Purpose
//!
//! The CL event service is the primary driver for proof generation. It listens for new head events
//! from the source CL client and initiates the proof workflow for each new block.

use std::{pin::pin, sync::Arc, time::Duration};

use futures::StreamExt;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    BlockStorage, ClClient, ClEvent, Error, HeadEvent,
    service::{Target, proof::ProofServiceMessage},
    subscribe_cl_events,
};

pub struct ClEventService {
    client: Arc<ClClient>,
    storage: Option<Arc<Mutex<BlockStorage>>>,
    proof_tx: mpsc::Sender<ProofServiceMessage>,
}

impl ClEventService {
    pub fn new(
        client: Arc<ClClient>,
        storage: Option<Arc<Mutex<BlockStorage>>>,
        proof_tx: mpsc::Sender<ProofServiceMessage>,
    ) -> Self {
        Self {
            client,
            storage,
            proof_tx,
        }
    }

    async fn handle_head(&self, head: HeadEvent) {
        debug!(slot = %head.slot, "Received ClEvent");

        let slot: u64 = match head.slot.parse() {
            Ok(slot) => slot,
            Err(e) => {
                warn!(
                    name = %self.client.name(),
                    error = %e,
                    slot = %head.slot,
                    "Invalid head slot value"
                );
                return;
            }
        };
        let block_root = head.block.clone();

        let execution_block_hash = match self.client.get_block_execution_hash(&block_root).await {
            Ok(Some(hash)) => hash,
            Ok(None) => {
                debug!(name = %self.client.name(), slot = slot, "No execution hash for block");
                return;
            }
            Err(e) => {
                debug!(name = %self.client.name(), error = %e, "Failed to get execution hash");
                return;
            }
        };

        if let Some(storage) = &self.storage {
            let mut storage_guard = storage.lock().await;
            if let Err(e) = storage_guard.save_cl_data(&execution_block_hash, slot, &block_root) {
                warn!(block_hash = %execution_block_hash, error = %e, "Failed to save CL block header to disk");
            } else {
                debug!(
                    block_hash = %execution_block_hash,
                    "Saved CL block header to disk"
                );
            }
        }

        let message = ProofServiceMessage::RequestProof {
            slot,
            block_root,
            execution_block_hash,
            target_clients: Target::All,
            target_proof_types: Target::All,
        };
        if let Err(error) = self.proof_tx.send(message).await {
            error!(slot = %slot, error = %error, "Failed to send proof request");
        }
    }

    pub async fn run(self, shutdown_token: CancellationToken) {
        const RECONNECT_DELAY: Duration = Duration::from_secs(5);

        loop {
            info!(name = %self.client.name(), "Connecting to CL SSE");

            let stream = match subscribe_cl_events(self.client.url()) {
                Ok(s) => s,
                Err(e) => {
                    error!(name = %self.client.name(), url = %self.client.url(), error = %e, "Invalid CL SSE url");
                    return;
                }
            };

            info!(name = %self.client.name(), "Subscribed to CL head events");
            let mut stream = pin!(stream);

            loop {
                tokio::select! {
                    biased;

                    _ = shutdown_token.cancelled() => {
                        info!(name = %self.client.name(), "ClEventService received shutdown signal");
                        return;
                    }

                    result = stream.next() => {
                        match result {
                            Some(Ok(ClEvent::Head(head))) => self.handle_head(head).await,
                            Some(Ok(ClEvent::Block(_))) => {}
                            Some(Err(e)) => {
                                if let Error::Sse(e) = &e && e.contains("ConnectionRefused") {
                                    break
                                }
                                error!(name = %self.client.name(), error = %e, "CL stream error")
                            },
                            None => break,
                        }
                    }
                }
            }

            warn!(name = %self.client.name(), "CL SSE stream ended, reconnecting in 5 seconds");
            tokio::select! {
                _ = shutdown_token.cancelled() => {
                    info!(name = %self.client.name(), "ClEventService received shutdown signal");
                    return;
                }

                _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            }
        }
    }
}

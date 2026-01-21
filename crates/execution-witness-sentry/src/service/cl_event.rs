use std::{pin::pin, sync::Arc};

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    ClClient, ClEvent,
    service::{Target, proof::ProofServiceMessage},
    subscribe_cl_events,
};

pub struct ClEventService {
    client: Arc<ClClient>,
    proof_tx: mpsc::Sender<ProofServiceMessage>,
}

impl ClEventService {
    pub fn new(client: Arc<ClClient>, proof_tx: mpsc::Sender<ProofServiceMessage>) -> Self {
        Self { client, proof_tx }
    }

    pub async fn run(self, shutdown_token: CancellationToken) {
        info!(name = %self.client.name(), "Connecting to CL SSE");

        let stream = match subscribe_cl_events(self.client.url()) {
            Ok(s) => s,
            Err(e) => {
                error!(name = %self.client.name(), error = %e, "Failed to subscribe to CL events");
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
                    break;
                }

                result = stream.next() => {
                    match result {
                        Some(Ok(ClEvent::Head(head))) => {
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
                                    continue;
                                }
                            };
                            let block_root = head.block.clone();

                            let execution_block_hash = match self.client
                                .get_block_execution_hash(&block_root)
                                .await
                            {
                                Ok(Some(hash)) => hash,
                                Ok(None) => {
                                    debug!(name = %self.client.name(), slot = slot, "No execution hash for block");
                                    continue;
                                }
                                Err(e) => {
                                    debug!(name = %self.client.name(), error = %e, "Failed to get execution hash");
                                    continue;
                                }
                            };

                            let message = ProofServiceMessage::RequestProof {
                                cl_name: self.client.name().to_string(),
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
                        Some(Ok(ClEvent::Block(_))) => {}
                        Some(Err(e)) => {
                            error!(name = %self.client.name(), error = %e, "CL stream error");
                        }
                        None => {
                            warn!(name = %self.client.name(), "CL SSE stream ended");
                            break;
                        }
                    }
                }
            }
        }
    }
}

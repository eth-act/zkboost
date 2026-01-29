//! # EL Event Service
//!
//! This module provides [`ElEventService`], which subscribes to EL head events via WebSocket and
//! triggers block data fetching.
//!
//! ## Purpose
//!
//! The EL event services notifies [`ElDataService`](super::el_data::ElDataService) when a new block
//! arrives, to retrieve the full block data and witness.

use std::{pin::pin, sync::Arc, time::Duration};

use futures::StreamExt;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{ElEndpoint, Header, service::el_data::ElDataServiceMessage, subscribe_blocks};

/// Subscribes to EL head events via WebSocket and triggers block data fetching.
///
/// This service maintains a WebSocket connection to an execution layer node, listening for
/// `newHeads` events. When a new block header arrives, it requests [`ElDataService`] to fetch
/// the full block data and execution witness.
///
/// [`ElDataService`]: super::el_data::ElDataService
pub struct ElEventService {
    /// Configuration for the EL endpoint (name and WebSocket URL).
    endpoint: ElEndpoint,
    /// Channel to send fetch requests to [`ElDataService`].
    el_data_tx: mpsc::Sender<ElDataServiceMessage>,
}

impl ElEventService {
    /// Creates a new EL event service for the given endpoint.
    pub fn new(endpoint: ElEndpoint, el_data_tx: mpsc::Sender<ElDataServiceMessage>) -> Self {
        Self {
            endpoint,
            el_data_tx,
        }
    }

    /// Processes an incoming block header by sending a fetch request to [`ElDataService`].
    async fn handle_header(&self, header: Header) {
        let block_hash = header.hash;
        info!(
            name = %self.endpoint.name,
            number = header.number,
            hash = %block_hash,
            "EL block header received"
        );

        let message = ElDataServiceMessage::FetchData { block_hash };
        if let Err(error) = self.el_data_tx.send(message).await {
            error!(block_hash = %block_hash, error = %error, "Failed to send block fetch request");
        }
    }

    /// Spawns the service as a background task.
    pub fn spawn(self: Arc<Self>, shutdown_token: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(self.run(shutdown_token))
    }

    /// Main event loop that processes EL head events until shutdown.
    async fn run(self: Arc<Self>, shutdown_token: CancellationToken) {
        const RECONNECT_DELAY: Duration = Duration::from_secs(2);

        let name = &self.endpoint.name;
        let ws_url = &self.endpoint.ws_url;

        loop {
            info!(name = %name, "Connecting to EL WebSocket");

            let stream = match subscribe_blocks(ws_url).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(name = %name, error = %e, "Failed to subscribe to EL, retrying in 5 seconds");
                    tokio::select! {
                        _ = shutdown_token.cancelled() => {
                            info!(name = %name, "ElEventService received shutdown signal");
                            break;
                        }
                        _ = tokio::time::sleep(RECONNECT_DELAY) => continue,
                    }
                }
            };

            info!(name = %name, "Subscribed to EL newHeads");
            let mut stream = pin!(stream);

            loop {
                tokio::select! {
                    biased;

                    _ = shutdown_token.cancelled() => {
                        info!(name = %name, "ElEventService received shutdown signal");
                        return;
                    }

                    result = stream.next() => {
                        match result {
                            Some(Ok(header)) => self.handle_header(header).await,
                            Some(Err(e)) => error!(name = %name, error = %e, "EL stream error"),
                            None => break,
                        }
                    }
                }
            }

            warn!(name = %name, "EL WebSocket stream ended, reconnecting");
        }
    }
}

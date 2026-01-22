//! # EL Event Service
//!
//! This module provides [`ElEventService`], which subscribes to EL head events via WebSocket and
//! triggers block data fetching.
//!
//! ## Purpose
//!
//! The EL event services notifies [`ElDataService`](super::el_data::ElDataService) when a new block
//! arrives, to retrieve the full block data and witness.

use std::{pin::pin, time::Duration};

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{ElEndpoint, Header, service::el_data::ElDataServiceMessage, subscribe_blocks};

pub struct ElEventService {
    endpoint: ElEndpoint,
    el_data_tx: mpsc::Sender<ElDataServiceMessage>,
}

impl ElEventService {
    pub fn new(endpoint: ElEndpoint, el_data_tx: mpsc::Sender<ElDataServiceMessage>) -> Self {
        Self {
            endpoint,
            el_data_tx,
        }
    }

    async fn handle_header(&self, header: Header) {
        let block_hash = header.hash.to_string();
        info!(
            name = %self.endpoint.name,
            number = header.number,
            hash = %block_hash,
            "EL block header received"
        );

        let message = ElDataServiceMessage::FetchData {
            block_hash: block_hash.clone(),
        };
        if let Err(error) = self.el_data_tx.send(message).await {
            error!(block_hash = %block_hash, error = %error, "Failed to send block fetch request");
        }
    }

    pub async fn run(self, shutdown_token: CancellationToken) {
        const RECONNECT_DELAY: Duration = Duration::from_secs(5);

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

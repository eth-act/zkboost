//! WebSocket subscription for new block headers.

use std::pin::Pin;
use std::task::{Context, Poll};

use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::Header;
use futures::Stream;

use crate::error::{Error, Result};

/// A stream of new block headers from an execution layer node.
///
/// This wraps an alloy provider subscription and keeps the provider alive
/// for the lifetime of the subscription.
pub struct BlockSubscription<P> {
    #[allow(dead_code)]
    provider: P,
    stream: Pin<Box<dyn Stream<Item = Header> + Send>>,
}

impl<P> std::fmt::Debug for BlockSubscription<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockSubscription").finish_non_exhaustive()
    }
}

impl<P> Unpin for BlockSubscription<P> {}

impl<P: Send> Stream for BlockSubscription<P> {
    type Item = Header;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

/// Subscribe to new block headers via WebSocket.
///
/// Connects to the execution layer node and subscribes to `newHeads` events.
pub async fn subscribe_blocks(ws_url: &str) -> Result<impl Stream<Item = Header> + Send> {
    let ws = WsConnect::new(ws_url);

    let provider = ProviderBuilder::new()
        .connect_ws(ws)
        .await
        .map_err(|e| Error::WebSocket(format!("connection failed: {e}")))?;

    let subscription = provider
        .subscribe_blocks()
        .await
        .map_err(|e| Error::WebSocket(format!("subscription failed: {e}")))?;

    Ok(BlockSubscription {
        provider,
        stream: Box::pin(subscription.into_stream()),
    })
}

//! WebSocket subscription for new block headers.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::Header;
use futures::Stream;
use url::Url;

use crate::error::{Error, Result};

/// Subscription stream that keeps the provider alive.
pub struct BlockSubscription<P> {
    #[allow(dead_code)]
    provider: P,
    stream: Pin<Box<dyn Stream<Item = Header> + Send>>,
}

impl<P> Unpin for BlockSubscription<P> {}

impl<P: Send> Stream for BlockSubscription<P> {
    type Item = Result<Header>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx).map(|opt| opt.map(Ok))
    }
}

/// Subscribe to new block headers via WebSocket.
pub async fn subscribe_blocks(ws_url: &Url) -> Result<impl Stream<Item = Result<Header>> + Send> {
    let ws = WsConnect::new(ws_url.as_str());
    let provider = ProviderBuilder::new()
        .connect_ws(ws)
        .await
        .map_err(|e| Error::WebSocket(format!("WebSocket connection failed: {e}")))?;

    let subscription = provider
        .subscribe_blocks()
        .await
        .map_err(|e| Error::WebSocket(format!("Block subscription failed: {e}")))?;

    let stream = Box::pin(subscription.into_stream());

    Ok(BlockSubscription { provider, stream })
}

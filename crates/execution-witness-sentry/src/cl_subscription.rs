//! SSE subscription for CL head events.

use std::pin::Pin;
use std::task::{Context, Poll};

use eventsource_client::{Client, SSE};
use futures::Stream;
use serde::Deserialize;
use url::Url;

use crate::error::{Error, Result};

/// Head event from the CL.
#[derive(Debug, Clone, Deserialize)]
pub struct HeadEvent {
    pub slot: String,
    pub block: String,
    pub state: String,
    pub epoch_transition: bool,
    pub execution_optimistic: bool,
}

/// Block event from the CL.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockEvent {
    pub slot: String,
    pub block: String,
    pub execution_optimistic: bool,
}

/// Unified CL event.
#[derive(Debug, Clone)]
pub enum ClEvent {
    Head(HeadEvent),
    Block(BlockEvent),
}

impl ClEvent {
    pub fn slot(&self) -> &str {
        match self {
            ClEvent::Head(e) => &e.slot,
            ClEvent::Block(e) => &e.slot,
        }
    }

    pub fn block_root(&self) -> &str {
        match self {
            ClEvent::Head(e) => &e.block,
            ClEvent::Block(e) => &e.block,
        }
    }
}

/// Stream of CL events.
pub struct ClEventStream {
    client: Pin<Box<dyn Stream<Item = eventsource_client::Result<SSE>> + Send>>,
}

impl Stream for ClEventStream {
    type Item = Result<ClEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.client.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(SSE::Event(event)))) => {
                    let result = match event.event_type.as_str() {
                        "head" => serde_json::from_str::<HeadEvent>(&event.data)
                            .map(ClEvent::Head)
                            .map_err(Error::Parse),
                        "block" => serde_json::from_str::<BlockEvent>(&event.data)
                            .map(ClEvent::Block)
                            .map_err(Error::Parse),
                        _ => continue,
                    };
                    return Poll::Ready(Some(result));
                }
                Poll::Ready(Some(Ok(SSE::Comment(_)))) => continue,
                Poll::Ready(Some(Ok(SSE::Connected(_)))) => continue,
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(Error::Sse(format!("{:?}", e)))));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Subscribe to CL head events via SSE.
pub fn subscribe_cl_events(base_url: &str) -> Result<ClEventStream> {
    let url = build_events_url(base_url)?;

    let client = eventsource_client::ClientBuilder::for_url(url.as_str())
        .map_err(|e| Error::Config(format!("Invalid SSE URL: {}", e)))?
        .build();

    Ok(ClEventStream {
        client: Box::pin(client.stream()),
    })
}

fn build_events_url(base_url: &str) -> Result<Url> {
    let base = Url::parse(base_url)?;
    Ok(base.join("/eth/v1/events?topics=head,block")?)
}

#[cfg(test)]
mod tests {
    use super::build_events_url;

    #[test]
    fn build_events_url_adds_path_without_trailing_slash() {
        let url = build_events_url("http://localhost:5052").unwrap();
        assert_eq!(
            url.as_str(),
            "http://localhost:5052/eth/v1/events?topics=head,block"
        );
    }

    #[test]
    fn build_events_url_adds_path_with_trailing_slash() {
        let url = build_events_url("http://localhost:5052/").unwrap();
        assert_eq!(
            url.as_str(),
            "http://localhost:5052/eth/v1/events?topics=head,block"
        );
    }
}

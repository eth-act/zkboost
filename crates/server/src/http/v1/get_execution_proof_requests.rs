//! SSE endpoint handler for `GET /v1/execution_proof_requests`.

use std::{convert::Infallible, pin::Pin, sync::Arc, time::Duration};

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tracing::instrument;
use zkboost_types::{ProofComplete, ProofEvent, ProofEventQuery};

use crate::http::{AppState, v1::Query};

/// SSE endpoint handler for `GET /v1/execution_proof_requests`.
///
/// Event delivery is at-least-once with latest-wins semantics. When subscribing with a
/// `new_payload_request_root`, terminal results (completions and failures) already in the
/// caches are replayed so events broadcast before the subscription are not lost. Replay can
/// race a concurrent retry: a stale `proof_failure` may be delivered before the
/// `proof_complete` of a retry that has already succeeded (a completion evicts the cached
/// failure, but a replay that started earlier can still emit it). Subscribers must treat the
/// most recent terminal event per `(new_payload_request_root, proof_type)` as authoritative.
#[instrument(skip_all)]
pub(crate) async fn get_execution_proof_requests(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ProofEventQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let proof_event_rx = state.proof_event_rx.resubscribe();

    let live_stream = BroadcastStream::new(proof_event_rx).filter_map(|result| result.ok());

    let merged: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        if let Some(new_payload_request_root) = params.new_payload_request_root {
            // Emit already-terminal results from the caches so the client does not miss events
            // that completed or failed before subscribing.
            let catch_up_events = {
                let mut events: Vec<ProofEvent> = {
                    let cache = state.proof_cache.read().await;
                    cache
                        .iter()
                        .filter(|((root, _), _)| *root == new_payload_request_root)
                        .map(|((new_payload_request_root, proof_type), _)| {
                            ProofComplete {
                                new_payload_request_root: *new_payload_request_root,
                                proof_type: *proof_type,
                            }
                            .into()
                        })
                        .collect()
                };
                let failures = state.failure_cache.read().await;
                events.extend(
                    failures
                        .iter()
                        .filter(|((root, _), _)| *root == new_payload_request_root)
                        .map(|(_, failure)| failure.clone().into()),
                );
                events
            };
            let catch_up_stream = tokio_stream::iter(catch_up_events);
            let filtered = catch_up_stream
                .chain(live_stream.filter(move |proof_event| {
                    proof_event.new_payload_request_root() == new_payload_request_root
                }))
                .map(|proof_event| Ok(to_axum_event(proof_event)));
            Box::pin(filtered)
        } else {
            let all = live_stream.map(|proof_event| Ok(to_axum_event(proof_event)));
            Box::pin(all)
        };

    Sse::new(merged).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn to_axum_event(proof_event: ProofEvent) -> Event {
    let (name, data) = proof_event.to_parts();
    Event::default().event(name).data(data)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{Router, body::Body, http::Request, routing::get};
    use tokio_stream::StreamExt;
    use tower::ServiceExt;
    use zkboost_types::{FailureReason, Hash256, ProofFailure, ProofType};

    use crate::http::{tests::mock_app_state, v1::get_execution_proof_requests};

    #[tokio::test]
    async fn test_sse_stream_opens() {
        let state = mock_app_state().await;
        let response = Router::new()
            .route(
                "/v1/execution_proof_requests",
                get(get_execution_proof_requests),
            )
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/execution_proof_requests")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.contains("text/event-stream"));
    }

    /// A subscriber arriving after a proof failed still receives the failure, replayed from
    /// the failure cache with the same event shape as the live broadcast.
    #[tokio::test]
    async fn test_subscribe_after_failure_replays_failure() {
        // Arrange: a terminal failure cached before anyone subscribes.
        let state = mock_app_state().await;
        let root = Hash256::repeat_byte(0xab);
        let failure = ProofFailure {
            new_payload_request_root: root,
            proof_type: ProofType::RethZisk,
            reason: FailureReason::ProvingError,
            error: "proving exploded".to_owned(),
        };
        state
            .failure_cache
            .write()
            .await
            .put((root, ProofType::RethZisk), failure.clone());

        // Act: subscribe filtered on the failed request's root.
        let response = Router::new()
            .route(
                "/v1/execution_proof_requests",
                get(get_execution_proof_requests),
            )
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/execution_proof_requests?new_payload_request_root={root}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // Assert: the first SSE frame is the replayed failure event.
        let mut body = response.into_body().into_data_stream();
        let frame = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("catch-up event should arrive immediately")
            .expect("stream should not end")
            .unwrap();
        let text = String::from_utf8(frame.to_vec()).unwrap();
        assert!(text.contains("event: proof_failure"), "{text}");
        let (_, expected_data) = zkboost_types::ProofEvent::ProofFailure(failure).to_parts();
        assert!(text.contains(&expected_data), "{text}");
    }
}

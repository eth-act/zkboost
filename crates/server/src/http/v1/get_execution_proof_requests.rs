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

#[instrument(skip_all)]
pub(crate) async fn get_execution_proof_requests(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ProofEventQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let proof_event_receiver = state.proof_event_receiver.resubscribe();

    let live_stream = BroadcastStream::new(proof_event_receiver).filter_map(|result| result.ok());

    let merged: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        if let Some(new_payload_request_root) = params.new_payload_request_root {
            // Emit already-completed proofs from cache so the client does not miss events that
            // completed before subscribing.
            let catch_up_events = {
                let cache = state.completed_proofs.read().await;
                cache
                    .iter()
                    .filter(|((cache, _), _)| *cache == new_payload_request_root)
                    .map(|((new_payload_request_root, proof_type), _)| {
                        ProofComplete {
                            new_payload_request_root: *new_payload_request_root,
                            proof_type: *proof_type,
                        }
                        .into()
                    })
                    .collect::<Vec<_>>()
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
    use axum::{Router, body::Body, http::Request, routing::get};
    use tower::ServiceExt;

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
}

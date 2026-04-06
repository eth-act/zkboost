//! Dashboard HTTP handlers: static HTML page, JSON state endpoint, and SSE event stream.

use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json,
    extract::State,
    response::{
        Html,
        sse::{Event, KeepAlive, Sse},
    },
};
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tracing::instrument;

use crate::{dashboard::DashboardStateResponse, http::AppState};

const DASHBOARD_HTML: &str = include_str!("dashboard/index.html");

#[instrument(skip_all)]
pub(crate) async fn get_dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

#[instrument(skip_all)]
pub(crate) async fn get_dashboard_state(
    State(state): State<Arc<AppState>>,
) -> Json<DashboardStateResponse> {
    let dashboard = state.dashboard.as_ref().expect("dashboard enabled");
    Json(dashboard.read().await.to_response())
}

#[instrument(skip_all)]
pub(crate) async fn get_dashboard_events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.dashboard_event_rx.resubscribe();
    let stream = BroadcastStream::new(rx)
        .filter_map(|result| result.ok())
        .map(|event| {
            let (name, data) = event.to_parts();
            Ok(Event::default().event(name).data(data))
        });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use crate::http::{router, tests::mock_app_state};

    #[tokio::test]
    async fn test_dashboard_html() {
        let state = mock_app_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
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
        assert!(content_type.contains("text/html"));
    }

    #[tokio::test]
    async fn test_dashboard_state() {
        let state = mock_app_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/dashboard/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["proofTypes"].is_array());
        assert!(json["historicalBlocks"].is_array());
        assert!(json["historySize"].is_number());
        assert!(json["buildVersion"].is_string());
    }

    #[tokio::test]
    async fn test_dashboard_events() {
        let state = mock_app_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/dashboard/events")
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

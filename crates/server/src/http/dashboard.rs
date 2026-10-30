//! Dashboard HTTP handlers serving the static HTML page, the JSON state endpoint, and the SSE
//! event stream.

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

/// Serves the dashboard HTML page.
#[instrument(skip_all)]
pub(crate) async fn get_dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// Returns a JSON snapshot of the dashboard state.
#[instrument(skip_all)]
pub(crate) async fn get_dashboard_state(
    State(state): State<Arc<AppState>>,
) -> Json<DashboardStateResponse> {
    let dashboard = state.dashboard.as_ref().expect("dashboard enabled");
    Json(dashboard.read().await.to_response())
}

/// Streams every dashboard event to the client as SSE.
#[instrument(skip_all)]
pub(crate) async fn get_dashboard_events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let dashboard_event_rx = state.dashboard_event_rx.resubscribe();
    let stream = BroadcastStream::new(dashboard_event_rx)
        // A lagging client has missed events, so its stream ends and it reconnects and reads a
        // fresh snapshot.
        .map_while(|result| result.ok())
        .map(|event| {
            let (name, data) = event.to_parts();
            Ok(Event::default().event(name).data(data))
        });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

//! HTTP service with the shared `AppState` and the Axum router. The router serves the Engine API
//! proxy, health, Prometheus metrics, and the optional dashboard, with request metrics middleware
//! and tracing.

use std::sync::Arc;

use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::{RwLock, broadcast};
use tower::ServiceBuilder;
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};

use crate::{
    dashboard::{DashboardEvent, DashboardState},
    engine::EngineProxyState,
    metrics::http_metrics_middleware,
};

mod dashboard;
mod engine;

/// Shared application state for all HTTP handlers.
pub(crate) struct AppState {
    pub(crate) engine: Arc<EngineProxyState>,
    pub(crate) metrics: PrometheusHandle,
    pub(crate) dashboard: Option<Arc<RwLock<DashboardState>>>,
    pub(crate) dashboard_event_rx: broadcast::Receiver<DashboardEvent>,
}

/// Creates the tracing span for an incoming HTTP request.
///
/// With the `otel` feature enabled, W3C trace context (`traceparent`/`tracestate`) is extracted
/// from the request headers and set as the span's parent, so spans created while handling the
/// request join the caller's distributed trace. Without the feature this only creates the span.
fn make_request_span(request: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    let span = tracing::info_span!(
        "request",
        method = %request.method(),
        uri = %request.uri(),
        version = ?request.version(),
        // Recorded only with the `otel` feature enabled, so the log format of default-feature
        // builds is unchanged.
        otel.kind = tracing::field::Empty,
    );
    #[cfg(feature = "otel")]
    {
        use tracing_opentelemetry::OpenTelemetrySpanExt;

        let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.extract(&crate::otel::HeaderExtractor(request.headers()))
        });
        // Fails only when the OpenTelemetry layer is not installed (no OTLP endpoint
        // configured), in which case there is no trace to join.
        let _ = span.set_parent(parent);
        span.record("otel.kind", "server");
        // OpenTelemetry HTTP semantic-convention attributes. Set as OTLP-only attributes
        // (not tracing fields) so they are exported without duplicating the log fields above.
        span.set_attribute("http.request.method", request.method().to_string());
        span.set_attribute("url.path", request.uri().path().to_owned());
        if let Some(query) = request.uri().query() {
            span.set_attribute("url.query", query.to_owned());
        }
    }
    span
}

/// Builds the Axum router with all endpoints and middleware.
pub(crate) fn router(state: Arc<AppState>) -> Router {
    let api_middleware = ServiceBuilder::new()
        .layer(middleware::from_fn(http_metrics_middleware))
        .layer(TraceLayer::new_for_http().make_span_with(make_request_span))
        .layer(CatchPanicLayer::new())
        .layer(DefaultBodyLimit::max(1 << 30));

    let api = Router::new()
        .route("/", post(engine::post_engine))
        .layer(api_middleware);

    let mut infra = Router::new()
        .route("/health", get(StatusCode::OK))
        .route("/metrics", get(get_metrics));

    if state.dashboard.is_some() {
        infra = infra
            .route("/dashboard", get(dashboard::get_dashboard))
            .route("/dashboard/state", get(dashboard::get_dashboard_state))
            .route("/dashboard/events", get(dashboard::get_dashboard_events));
    }

    api.merge(infra).with_state(state)
}

async fn get_metrics(State(state): State<Arc<AppState>>) -> String {
    state.metrics.render()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::HashMap, sync::Arc};

    use axum::{body::Body, http::Request};
    use metrics_exporter_prometheus::PrometheusBuilder;
    use tokio::sync::{RwLock, broadcast, mpsc};
    use tower::ServiceExt;
    use zkboost_types::ProofType;

    use crate::{
        config::{Config, DashboardConfig, MockProvingTime, zkVMConfig},
        dashboard::DashboardState,
        engine::EngineProxyState,
        http::{AppState, router},
    };

    pub(crate) async fn mock_app_state() -> Arc<AppState> {
        let proof_type = ProofType::RethZisk;
        // Nothing listens on port 1, so every EL call fails fast.
        let config = Config {
            port: 0,
            el_engine_endpoint: "http://127.0.0.1:1/".parse().unwrap(),
            cl_beacon_endpoint: "http://127.0.0.1:1/".parse().unwrap(),
            validator_keystore_path: concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixture/voting-keystore.json"
            )
            .into(),
            validator_keystore_password_path: concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixture/voting-keystore-password"
            )
            .into(),
            dashboard: DashboardConfig::default(),
            zkvm: vec![zkVMConfig::Mock {
                proof_type,
                proof_timeout_secs: 12,
                mock_proving_time: MockProvingTime::Constant { ms: 10 },
                mock_failure: false,
            }],
        };
        let (worker_input_tx, _) = mpsc::channel(16);
        let (dashboard_service_tx, _) = mpsc::channel(16);
        let engine = EngineProxyState::new(
            &config,
            HashMap::from_iter([(proof_type, worker_input_tx)]),
            dashboard_service_tx,
        )
        .unwrap();

        let metrics = PrometheusBuilder::new().build_recorder().handle();
        let dashboard = Arc::new(RwLock::new(DashboardState::new(vec![proof_type], 256))).into();
        let (_, dashboard_event_rx) = broadcast::channel(16);

        Arc::new(AppState {
            engine: Arc::new(engine),
            metrics,
            dashboard,
            dashboard_event_rx,
        })
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let state = mock_app_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }

    // The `otel` request-span propagation test lives in its own integration-test binary
    // (`tests/otel_request_span.rs`): it needs a tracing subscriber that observes spans created
    // on other tasks, and sharing a process with parallel tests poisons the global callsite
    // interest cache.
}

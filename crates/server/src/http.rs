//! HTTP service: `AppState`, Axum router with v1 API handlers, Prometheus metrics middleware, and
//! request tracing.

use std::{collections::HashMap, sync::Arc};

use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use bytes::Bytes;
use lru::LruCache;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::{RwLock, broadcast, mpsc};
use tower::ServiceBuilder;
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};
use zkboost_types::{Hash256, ProofEvent, ProofType};

use crate::{
    dashboard::{DashboardEvent, DashboardState},
    metrics::http_metrics_middleware,
    proof::{ProofServiceMessage, zkvm::zkVMInstance},
};

mod dashboard;
mod v1;

/// Shared application state for all HTTP handlers.
pub(crate) struct AppState {
    pub(crate) zkvms: Arc<HashMap<ProofType, zkVMInstance>>,
    pub(crate) proof_cache: Arc<RwLock<LruCache<(Hash256, ProofType), Bytes>>>,
    pub(crate) metrics: PrometheusHandle,
    pub(crate) dashboard: Option<Arc<RwLock<DashboardState>>>,
    pub(crate) proof_service_tx: mpsc::Sender<ProofServiceMessage>,
    pub(crate) proof_event_rx: broadcast::Receiver<ProofEvent>,
    pub(crate) dashboard_event_rx: broadcast::Receiver<DashboardEvent>,
}

impl AppState {
    /// Creates shared application state for the HTTP handlers.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        zkvms: Arc<HashMap<ProofType, zkVMInstance>>,
        proof_cache: Arc<RwLock<LruCache<(Hash256, ProofType), Bytes>>>,
        metrics: PrometheusHandle,
        dashboard: Option<Arc<RwLock<DashboardState>>>,
        proof_service_tx: mpsc::Sender<ProofServiceMessage>,
        proof_event_rx: broadcast::Receiver<ProofEvent>,
        dashboard_event_rx: broadcast::Receiver<DashboardEvent>,
    ) -> Self {
        Self {
            zkvms,
            proof_cache,
            metrics,
            dashboard,
            proof_service_tx,
            proof_event_rx,
            dashboard_event_rx,
        }
    }
}

/// Builds the Axum router with all endpoints and middleware.
pub(crate) fn router(state: Arc<AppState>) -> Router {
    let api_middleware = ServiceBuilder::new()
        .layer(middleware::from_fn(http_metrics_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .layer(DefaultBodyLimit::max(1 << 30));

    let api = Router::new()
        .route(
            "/v1/execution_proof_requests",
            post(v1::post_execution_proof_requests).get(v1::get_execution_proof_requests),
        )
        .route(
            "/v1/execution_proofs/{new_payload_request_root}/{proof_type}",
            get(v1::get_execution_proofs),
        )
        .route(
            "/v1/execution_proof_verifications",
            post(v1::post_execution_proof_verifications),
        )
        .fallback(fallback_handler)
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

async fn fallback_handler() -> v1::ErrorResponse {
    v1::ErrorResponse::not_found("route not found")
}

async fn get_metrics(State(state): State<Arc<AppState>>) -> String {
    state.metrics.render()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};

    use axum::{body::Body, http::Request};
    use lru::LruCache;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use tokio::sync::{RwLock, broadcast, mpsc};
    use tower::ServiceExt;
    use zkboost_types::ProofType;

    use crate::{
        config::{MockProvingTime, zkVMConfig},
        dashboard::DashboardState,
        http::{AppState, router},
        proof::zkvm::zkVMInstance,
    };

    pub(crate) async fn mock_app_state() -> Arc<AppState> {
        let proof_type = ProofType::RethZisk;
        let mock_config = zkVMConfig::Mock {
            proof_type,
            proof_timeout_secs: 12,
            mock_proving_time: MockProvingTime::Constant { ms: 10 },
            mock_proof_size: 64,
            mock_failure: false,
        };
        let zkvm = zkVMInstance::new(&mock_config).await.unwrap();
        let zkvms = Arc::new(HashMap::from_iter([(proof_type, zkvm)]));

        let proof_cache = Arc::new(RwLock::new(LruCache::new(NonZeroUsize::new(128).unwrap())));

        let metrics = PrometheusBuilder::new().build_recorder().handle();
        let dashboard = Arc::new(RwLock::new(DashboardState::new(vec![proof_type], 256))).into();

        let (proof_service_tx, _) = mpsc::channel(16);
        let (_, proof_event_rx) = broadcast::channel(16);
        let (_, dashboard_event_rx) = broadcast::channel(16);

        Arc::new(AppState::new(
            zkvms,
            proof_cache,
            metrics,
            dashboard,
            proof_service_tx,
            proof_event_rx,
            dashboard_event_rx,
        ))
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

    #[tokio::test]
    async fn test_unknown_route_returns_json_404() {
        let state = mock_app_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], 404);
        assert_eq!(json["message"], "route not found");
    }
}

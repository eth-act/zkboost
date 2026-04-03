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
use tower_http::trace::TraceLayer;
use zkboost_types::{Hash256, ProofEvent, ProofType};

use crate::{
    metrics::http_metrics_middleware,
    proof::{ProofServiceMessage, zkvm::zkVMInstance},
};

mod v1;

/// Shared application state for all HTTP handlers.
#[allow(missing_debug_implementations)]
pub(crate) struct AppState {
    pub(crate) zkvms: Arc<HashMap<ProofType, zkVMInstance>>,
    pub(crate) completed_proofs: Arc<RwLock<LruCache<(Hash256, ProofType), Bytes>>>,
    pub(crate) metrics: PrometheusHandle,
    pub(crate) proof_service_tx: mpsc::Sender<ProofServiceMessage>,
    pub(crate) proof_event_rx: broadcast::Receiver<ProofEvent>,
}

impl AppState {
    /// Creates shared application state for the HTTP handlers.
    pub(crate) fn new(
        zkvms: Arc<HashMap<ProofType, zkVMInstance>>,
        completed_proofs: Arc<RwLock<LruCache<(Hash256, ProofType), Bytes>>>,
        metrics: PrometheusHandle,
        proof_service_tx: mpsc::Sender<ProofServiceMessage>,
        proof_event_rx: broadcast::Receiver<ProofEvent>,
    ) -> Self {
        Self {
            zkvms,
            completed_proofs,
            metrics,
            proof_service_tx,
            proof_event_rx,
        }
    }
}

/// Builds the Axum router with all endpoints and middleware.
pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
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
        .route("/health", get(StatusCode::OK))
        .route("/metrics", get(get_metrics))
        .fallback(fallback_handler)
        .with_state(state)
        .layer(middleware::from_fn(http_metrics_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(1 << 30))
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
    use zkboost_types::{ProofEvent, ProofType};

    use crate::{
        config::{MockProvingTime, zkVMConfig},
        http::{AppState, router},
        proof::{ProofServiceMessage, zkvm::zkVMInstance},
    };

    pub(crate) async fn mock_app_state() -> Arc<AppState> {
        let completed_proofs =
            Arc::new(RwLock::new(LruCache::new(NonZeroUsize::new(128).unwrap())));
        let (_, proof_event_rx) = broadcast::channel::<ProofEvent>(16);
        let (proof_service_tx, _) = mpsc::channel::<ProofServiceMessage>(16);

        let mock_config = zkVMConfig::Mock {
            proof_type: ProofType::RethZisk,
            mock_proving_time: MockProvingTime::Constant { ms: 10 },
            mock_proof_size: 64,
            mock_failure: false,
        };
        let zkvm = zkVMInstance::new(&mock_config).await.unwrap();
        let zkvms = Arc::new(HashMap::from_iter([(ProofType::RethZisk, zkvm)]));

        let metrics = PrometheusBuilder::new().build_recorder().handle();

        Arc::new(AppState::new(
            zkvms,
            completed_proofs,
            metrics,
            proof_service_tx,
            proof_event_rx,
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

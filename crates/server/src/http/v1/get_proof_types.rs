//! Handler for `GET /v1/proof_types`.

use std::sync::Arc;

use axum::{Json, extract::State, response::IntoResponse};
use tracing::instrument;
use zkboost_types::{ProofTypeInfo, ProofTypesResponse};

use super::ErrorResponse;
use crate::http::AppState;

/// Returns the list of initialized proof types with their capabilities.
#[instrument(skip_all)]
pub(crate) async fn get_proof_types(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ErrorResponse> {
    let mut proof_types: Vec<ProofTypeInfo> = state
        .zkvms
        .iter()
        .map(|(proof_type, instance)| {
            let (kind, can_prove, can_verify) = instance.backend_capabilities();
            ProofTypeInfo {
                proof_type: *proof_type,
                kind,
                can_prove,
                can_verify,
            }
        })
        .collect();

    // Sort by proof_type for deterministic response order.
    proof_types.sort_by_key(|info| info.proof_type);

    Ok(Json(ProofTypesResponse { proof_types }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::Request,
        routing::get,
    };
    use tower::ServiceExt;
    use zkboost_types::{BackendKind, ProofType, ProofTypesResponse};

    use crate::http::{AppState, v1::get_proof_types};

    fn test_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/v1/proof_types", get(get_proof_types))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_proof_types_returns_configured_backends() {
        // mock_app_state() creates one RethZisk mock backend
        let state = crate::http::tests::mock_app_state().await;

        let response = test_router(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/proof_types")
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
        assert!(content_type.contains("application/json"));

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response: ProofTypesResponse = serde_json::from_slice(&body).unwrap();

        assert_eq!(response.proof_types.len(), 1);

        let info = &response.proof_types[0];
        assert_eq!(info.proof_type, ProofType::RethZisk);
        assert_eq!(info.kind, BackendKind::Mock);
        assert!(info.can_prove);
        assert!(info.can_verify);
    }

    #[tokio::test]
    async fn test_proof_types_json_field_names() {
        let state = crate::http::tests::mock_app_state().await;

        let response = test_router(state)
            .oneshot(
                Request::builder()
                    .uri("/v1/proof_types")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Assert exact JSON field names for serialization stability
        assert!(json.get("proof_types").is_some());
        let proof_types = json["proof_types"].as_array().unwrap();
        assert!(!proof_types.is_empty());

        let first = &proof_types[0];
        assert!(first.get("proof_type").is_some());
        assert!(first.get("kind").is_some());
        assert!(first.get("can_prove").is_some());
        assert!(first.get("can_verify").is_some());

        // Assert kind serializes to lowercase string
        assert_eq!(first["kind"], "mock");
    }
}

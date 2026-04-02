//! Handler for `GET /v1/execution_proofs/{new_payload_request_root}/{proof_type}`.

use std::sync::Arc;

use axum::{extract::State, response::IntoResponse};
use tracing::instrument;
use zkboost_types::{Hash256, ProofType};

use crate::http::{
    AppState,
    v1::{ErrorResponse, Path},
};

#[instrument(skip_all)]
pub(crate) async fn get_execution_proofs(
    State(state): State<Arc<AppState>>,
    Path((new_payload_request_root, proof_type)): Path<(Hash256, ProofType)>,
) -> Result<impl IntoResponse, ErrorResponse> {
    match state
        .completed_proofs
        .read()
        .await
        .peek(&(new_payload_request_root, proof_type))
    {
        Some(proof) => Ok(proof.clone()),
        None => Err(ErrorResponse::not_found(format!(
            "proof not found for root {new_payload_request_root} and type {proof_type}"
        ))),
    }
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
    use bytes::Bytes;
    use tower::ServiceExt;
    use zkboost_types::{Hash256, ProofType};

    use crate::http::{AppState, tests::mock_app_state, v1::get_execution_proofs};

    fn test_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route(
                "/v1/execution_proofs/{new_payload_request_root}/{proof_type}",
                get(get_execution_proofs),
            )
            .with_state(state)
    }

    #[tokio::test]
    async fn test_proof_not_found() {
        let state = mock_app_state().await;
        let response = test_router(state)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/execution_proofs/{}/ethrex-zisk",
                        Hash256::ZERO
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.contains("application/json"));

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], 404);
        assert!(json.get("message").is_some());
    }

    #[tokio::test]
    async fn test_proof_found() {
        let state = mock_app_state().await;
        let new_payload_request_root = Hash256::from_slice(&[1u8; 32]);
        let proof_type = ProofType::EthrexZisk;
        let proof = Bytes::from(vec![42u8; 64]);
        state
            .completed_proofs
            .write()
            .await
            .put((new_payload_request_root, proof_type), proof.clone());

        let response = test_router(state)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/execution_proofs/{new_payload_request_root}/ethrex-zisk"
                    ))
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
        assert!(content_type.contains("application/octet-stream"));

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), &[42u8; 64]);
    }
}

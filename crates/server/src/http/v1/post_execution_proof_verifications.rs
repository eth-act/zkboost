//! Handler for `POST /v1/execution_proof_verifications`.

use std::{sync::Arc, time::Instant};

use axum::{Json, extract::State};
use bytes::Bytes;
use tracing::{instrument, warn};
use zkboost_types::{ProofStatus, ProofVerificationQuery, ProofVerificationResponse};

use crate::{
    http::{
        AppState,
        v1::{ErrorResponse, Query},
    },
    metrics::record_verify,
};

#[instrument(skip_all)]
pub(crate) async fn post_execution_proof_verifications(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ProofVerificationQuery>,
    body: Bytes,
) -> Result<Json<ProofVerificationResponse>, ErrorResponse> {
    let start = Instant::now();
    let proof_type = params.proof_type;

    let zkvm = state.zkvms.get(&proof_type).ok_or_else(|| {
        record_verify(proof_type, false, start.elapsed());
        ErrorResponse::not_found(format!("unknown proof_type: {proof_type}"))
    })?;

    let status = match zkvm
        .verify(params.new_payload_request_root, body.to_vec())
        .await
    {
        Ok(()) => ProofStatus::Valid,
        Err(e) => {
            warn!(proof_type = %proof_type, error = %e, "verification failed");
            ProofStatus::Invalid
        }
    };

    record_verify(proof_type, status.is_valid(), start.elapsed());

    Ok(Json(ProofVerificationResponse { status }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::Request,
        routing::post,
    };
    use bytes::Bytes;
    use tower::ServiceExt;
    use zkboost_types::{ElKind, Hash256, ProofStatus, ProofVerificationResponse};

    use crate::{
        http::{AppState, tests::mock_app_state, v1::post_execution_proof_verifications},
        proof::zkvm::{MockProof, expected_public_values},
    };

    fn test_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route(
                "/v1/execution_proof_verifications",
                post(post_execution_proof_verifications),
            )
            .with_state(state)
    }

    #[tokio::test]
    async fn test_unknown_proof_type_returns_not_found() {
        let state = mock_app_state().await;
        let body = mock_proof(Hash256::ZERO, 64);
        let response = test_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/v1/execution_proof_verifications?proof_type=ethrex-risc0&new_payload_request_root={}",
                        Hash256::ZERO
                    ))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn test_valid_mock_proof() {
        let state = mock_app_state().await;
        let body = mock_proof(Hash256::ZERO, 64);
        let response = test_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/v1/execution_proof_verifications?proof_type=reth-zisk&new_payload_request_root={}",
                        Hash256::ZERO
                    ))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let resp: ProofVerificationResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp.status, ProofStatus::Valid);
    }

    #[tokio::test]
    async fn test_invalid_mock_proof() {
        let state = mock_app_state().await;
        let body = mock_proof(Hash256::ZERO, 32);
        let response = test_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/v1/execution_proof_verifications?proof_type=reth-zisk&new_payload_request_root={}",
                        Hash256::ZERO
                    ))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let resp: ProofVerificationResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp.status, ProofStatus::Invalid);
    }

    fn mock_proof(new_payload_request_root: Hash256, mock_proof_size: u64) -> Bytes {
        let public_values = expected_public_values(new_payload_request_root, ElKind::Reth).unwrap();
        bincode::serialize(&MockProof::new(public_values.to_vec(), mock_proof_size))
            .unwrap()
            .into()
    }
}

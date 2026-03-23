//! Handler for `POST /v1/execution_proof_requests`.

use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use bytes::Bytes;
use tracing::instrument;
use zkboost_types::{
    Decode, MainnetEthSpec, NewPayloadRequest, ProofRequestQuery, ProofRequestResponse, TreeHash,
};

use crate::{
    http::{
        AppState,
        v1::{ErrorResponse, Query},
    },
    proof::ProofServiceMessage,
};

#[instrument(skip_all)]
pub(crate) async fn post_execution_proof_requests(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ProofRequestQuery>,
    body: Bytes,
) -> Result<(StatusCode, Json<ProofRequestResponse>), ErrorResponse> {
    let new_payload_request = NewPayloadRequest::<MainnetEthSpec>::from_ssz_bytes(&body)
        .map_err(|e| ErrorResponse::bad_request(format!("invalid SSZ body: {e:?}")))?;

    let new_payload_request_root = new_payload_request.tree_hash_root();

    for proof_type in &params.proof_types {
        if !state.zkvms.contains_key(proof_type) {
            return Err(ErrorResponse::bad_request(format!(
                "no zkVM configured for proof type '{proof_type}'"
            )));
        }
    }

    state
        .proof_service_tx
        .send(ProofServiceMessage::RequestProof {
            new_payload_request: Arc::new(new_payload_request),
            proof_types: params.proof_types,
        })
        .await
        .map_err(|e| {
            ErrorResponse::internal_server_error(format!("failed to enqueue proof: {e}"))
        })?;

    Ok((
        StatusCode::OK,
        Json(ProofRequestResponse {
            new_payload_request_root,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{Router, body::Body, http::Request, routing::post};
    use tower::ServiceExt;

    use crate::http::{AppState, tests::mock_app_state, v1::post_execution_proof_requests};

    fn test_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route(
                "/v1/execution_proof_requests",
                post(post_execution_proof_requests),
            )
            .with_state(state)
    }

    #[tokio::test]
    async fn test_bad_ssz_body() {
        let state = mock_app_state().await;
        let response = test_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/execution_proof_requests?proof_types=ethrex-zisk")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(vec![0u8; 16]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }

    #[tokio::test]
    async fn test_unknown_proof_type_returns_bad_request() {
        let state = mock_app_state().await;
        let response = test_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/execution_proof_requests?proof_types=ethrex-zisk")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(vec![0u8; 16]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
}

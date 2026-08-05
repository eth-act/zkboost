//! Handler for `POST /v1/execution_proof_verifications`.

use std::{sync::Arc, time::Instant};

use axum::{Json, extract::State};
use bytes::Bytes;
use tracing::{instrument, warn};
use zkboost_types::{ProofStatus, ProofVerificationBody, ProofVerificationResponse, SszDecode};

use crate::{
    http::{AppState, v1::ErrorResponse},
    metrics::record_verify,
};

#[instrument(skip_all)]
pub(crate) async fn post_execution_proof_verifications(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Json<ProofVerificationResponse>, ErrorResponse> {
    let start = Instant::now();

    let request = ProofVerificationBody::from_ssz_bytes(&body)
        .map_err(|e| ErrorResponse::bad_request(format!("invalid SSZ body: {e:?}")))?;
    let proof_type = request.proof_type;

    let zkvm = state.zkvms.get(&proof_type).ok_or_else(|| {
        record_verify(proof_type, false, start.elapsed());
        ErrorResponse::not_found(format!("unknown proof_type: {proof_type}"))
    })?;

    let status = match zkvm
        .verify(
            request.new_payload_request_root.into(),
            &request.chain_config,
            request.proof,
        )
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
        http::{Request, Response, StatusCode},
        routing,
    };
    use tower::ServiceExt;
    use zkboost_types::{
        ChainConfig, ForkActivation, ForkConfig, Hash256, ProofStatus, ProofType,
        ProofVerificationBody, ProofVerificationResponse, ProtocolFork, SszEncode,
    };

    use crate::{
        http::{AppState, tests::mock_app_state, v1::post_execution_proof_verifications},
        proof::zkvm::{MockProof, expected_public_values},
    };

    fn bpo2_config() -> ChainConfig {
        ChainConfig {
            chain_id: 1,
            active_fork: ForkConfig::new(ForkActivation::new(None, Some(0))),
        }
    }

    fn mock_proof(chain_config: &ChainConfig, root: Hash256, size: usize) -> Vec<u8> {
        MockProof::new(expected_public_values(root, chain_config), size)
            .to_bytes()
            .unwrap()
    }

    fn verification_body(proof_type: ProofType, root: Hash256, proof: Vec<u8>) -> Vec<u8> {
        ProofVerificationBody {
            fork: ProtocolFork::BPO2,
            new_payload_request_root: root.0,
            chain_config: bpo2_config(),
            proof_type,
            proof,
        }
        .to_ssz()
    }

    async fn send(state: Arc<AppState>, body: Vec<u8>) -> Response<Body> {
        Router::new()
            .route(
                "/v1/execution_proof_verifications",
                routing::post(post_execution_proof_verifications),
            )
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/execution_proof_verifications")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn status(response: Response<Body>) -> ProofStatus {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice::<ProofVerificationResponse>(&body)
            .unwrap()
            .status
    }

    #[tokio::test]
    async fn test_unknown_proof_type_returns_not_found() {
        let proof = mock_proof(&bpo2_config(), Hash256::ZERO, 256);
        let body = verification_body(ProofType::EthrexOpenVM, Hash256::ZERO, proof);
        let response = send(mock_app_state().await, body).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_valid_mock_proof() {
        let proof = mock_proof(&bpo2_config(), Hash256::ZERO, 256);
        let body = verification_body(ProofType::RethZisk, Hash256::ZERO, proof);
        let response = send(mock_app_state().await, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(status(response).await, ProofStatus::Valid);
    }

    #[tokio::test]
    async fn test_invalid_mock_proof() {
        let body = verification_body(ProofType::RethZisk, Hash256::ZERO, vec![0; 31]);
        let response = send(mock_app_state().await, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(status(response).await, ProofStatus::Invalid);
    }

    #[tokio::test]
    async fn test_bad_ssz_body() {
        let response = send(mock_app_state().await, vec![0u8; 8]).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

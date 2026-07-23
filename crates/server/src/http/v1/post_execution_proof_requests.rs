//! Handler for `POST /v1/execution_proof_requests`.

use std::{collections::HashSet, sync::Arc};

use axum::{Json, extract::State};
use bytes::Bytes;
use tracing::{debug, info_span, instrument};
use zkboost_types::{
    Hash256, HashTreeRoot, ProofRequestBody, ProofRequestResponse, Sha2Hasher, SszDecode,
};

use crate::{
    chain_config::complete_chain_config,
    http::{AppState, v1::ErrorResponse},
    proof::{ProofServiceMessage, zkvm::zkVMInstance},
};

#[instrument(skip_all)]
pub(crate) async fn post_execution_proof_requests(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Json<ProofRequestResponse>, ErrorResponse> {
    let request = ProofRequestBody::from_ssz_bytes(&body)
        .map_err(|e| ErrorResponse::bad_request(format!("invalid SSZ body: {e:?}")))?;

    if request.proof_types.is_empty() {
        return Err(ErrorResponse::bad_request(
            "empty proof types in request".to_string(),
        ));
    }

    let proof_types = HashSet::from_iter(request.proof_types.iter().copied());
    if proof_types.len() != request.proof_types.len() {
        return Err(ErrorResponse::bad_request(
            "duplicate proof types in request".to_string(),
        ));
    }

    for proof_type in &proof_types {
        if !state.zkvms.contains_key(proof_type) {
            return Err(ErrorResponse::bad_request(format!(
                "no zkVM configured for proof type '{proof_type}'"
            )));
        }
    }

    // Reject proof generation requests for verifier-only instances early,
    // before wasting resources on witness fetching.
    for proof_type in &proof_types {
        if let Some(zkvm) = state.zkvms.get(proof_type)
            && matches!(zkvm, zkVMInstance::Verifier { .. })
        {
            debug!(
                %proof_type,
                "rejecting proof request: verifier-only instance"
            );
            return Err(ErrorResponse::bad_request(format!(
                "proof generation not supported for verifier-only zkvm '{proof_type}'"
            )));
        }
    }

    let chain_config = complete_chain_config(&request.chain_config, &state.blob_params)
        .map_err(|e| ErrorResponse::bad_request(e.to_string()))?;

    let new_payload_request = Arc::new(request.new_payload_request);
    let new_payload_request_root = Hash256::from(new_payload_request.hash_tree_root(&Sha2Hasher));
    let block_number = new_payload_request.block_number();
    let timestamp = new_payload_request.timestamp();
    let gas_used = new_payload_request.gas_used();

    let span = info_span!(
        "request_proof",
        new_payload_request_root = %new_payload_request_root,
        block_number,
        timestamp,
        gas_used
    );

    state
        .proof_service_tx
        .send(ProofServiceMessage::RequestProof {
            new_payload_request_root,
            new_payload_request,
            chain_config,
            proof_types,
            span,
        })
        .await
        .map_err(|e| {
            ErrorResponse::internal_server_error(format!("failed to enqueue proof: {e}"))
        })?;

    Ok(Json(ProofRequestResponse {
        new_payload_request_root,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing,
    };
    use tower::ServiceExt;
    use zkboost_types::{
        BlobSchedule, ChainConfig, ForkActivation, ForkConfig, NewPayloadRequest, ProofRequestBody,
        ProofType, ProtocolFork, SszDecode, SszEncode,
    };

    use crate::{
        chain_config::BlobParams,
        http::{
            AppState,
            tests::{mock_app_state, mock_app_state_with_blob_params},
            v1::post_execution_proof_requests,
        },
    };

    const NEW_PAYLOAD_REQUEST: &[u8] =
        include_bytes!("../../../tests/fixture/new_payload_request.ssz");

    const BPO2_SCHEDULE: BlobSchedule = {
        let params = alloy_eips::eip7840::BlobParams::bpo2();
        BlobSchedule {
            target: params.target_blob_count,
            max: params.max_blob_count,
            base_fee_update_fraction: params.update_fraction as u64,
        }
    };

    fn proof_request_body(proof_types: Vec<ProofType>) -> Vec<u8> {
        let new_payload_request = NewPayloadRequest::from_ssz_bytes(NEW_PAYLOAD_REQUEST).unwrap();
        let chain_config = ChainConfig {
            chain_id: 1,
            active_fork: ForkConfig::new(
                ProtocolFork::BPO2,
                ForkActivation::new(None, Some(0)),
                None,
            ),
        };
        ProofRequestBody {
            proof_types,
            new_payload_request,
            chain_config,
        }
        .to_ssz()
    }

    async fn send(state: Arc<AppState>, body: Vec<u8>) -> StatusCode {
        Router::new()
            .route(
                "/v1/execution_proof_requests",
                routing::post(post_execution_proof_requests),
            )
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/execution_proof_requests")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn test_bad_ssz_body() {
        assert_eq!(send(mock_app_state().await, vec![0u8; 16]).await, 400);
    }

    #[tokio::test]
    async fn test_empty_proof_types_returns_bad_request() {
        let body = proof_request_body(vec![]);
        assert_eq!(send(mock_app_state().await, body).await, 400);
    }

    #[tokio::test]
    async fn test_duplicate_proof_types_returns_bad_request() {
        let body = proof_request_body(vec![ProofType::RethZisk, ProofType::RethZisk]);
        assert_eq!(send(mock_app_state().await, body).await, 400);
    }

    #[tokio::test]
    async fn test_unknown_proof_type_returns_bad_request() {
        let body = proof_request_body(vec![ProofType::EthrexOpenVM]);
        assert_eq!(send(mock_app_state().await, body).await, 400);
    }

    #[tokio::test]
    async fn test_blob_max_mismatch_returns_bad_request() {
        let blob_params = BlobParams::from([(ProtocolFork::BPO2, BPO2_SCHEDULE)]);
        let chain_config = ChainConfig {
            chain_id: 1,
            active_fork: ForkConfig::new(
                ProtocolFork::BPO2,
                ForkActivation::new(None, Some(0)),
                Some(BlobSchedule {
                    target: 0,
                    max: BPO2_SCHEDULE.max + 1,
                    base_fee_update_fraction: 0,
                }),
            ),
        };
        let new_payload_request = NewPayloadRequest::from_ssz_bytes(NEW_PAYLOAD_REQUEST).unwrap();
        let body = ProofRequestBody {
            proof_types: vec![ProofType::RethZisk],
            new_payload_request,
            chain_config,
        }
        .to_ssz();
        let state = mock_app_state_with_blob_params(blob_params).await;
        assert_eq!(send(state, body).await, 400);
    }
}

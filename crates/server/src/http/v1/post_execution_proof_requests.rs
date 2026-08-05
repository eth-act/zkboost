//! Handler for `POST /v1/execution_proof_requests`.

use std::{collections::HashSet, sync::Arc};

use axum::{Json, extract::State};
use bytes::Bytes;
use tracing::{debug, info_span, instrument};
use zkboost_types::{
    Hash256, HashTreeRoot, NewPayloadRequest, ProofRequestBody, ProofRequestResponse, ProtocolFork,
    Sha2Hasher, SszDecode,
};

use crate::{
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

    if !is_valid_payload_variant(request.fork, &request.new_payload_request) {
        return Err(ErrorResponse::bad_request(format!(
            "submitted payload is not valid variant for fork {:?}",
            request.fork
        )));
    }

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
            fork: request.fork,
            new_payload_request_root,
            new_payload_request,
            chain_config: request.chain_config,
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

/// Returns whether the payload variant is valid for the fork, mirroring the
/// partition the guest applies when it decodes a schema-prefixed stateless input.
///
/// A mismatch makes the guest reject the input and return a default result, which the prover
/// still turns into a valid proof of that empty result, so the request is refused here instead.
fn is_valid_payload_variant(fork: ProtocolFork, new_payload_request: &NewPayloadRequest) -> bool {
    use NewPayloadRequest::*;
    use ProtocolFork::*;
    matches!(
        (fork, new_payload_request),
        (Paris, Bellatrix(_))
            | (Shanghai, Capella(_))
            | (Cancun, Deneb(_))
            | (Prague | Osaka | BPO1 | BPO2, ElectraFulu(_))
            | (Amsterdam, Gloas(_))
    )
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
        ChainConfig, ForkActivation, ForkConfig, NewPayloadRequest, ProofRequestBody, ProofType,
        ProtocolFork, SszDecode, SszEncode,
    };

    use crate::http::{AppState, tests::mock_app_state, v1::post_execution_proof_requests};

    const NEW_PAYLOAD_REQUEST: &[u8] =
        include_bytes!("../../../tests/fixture/new_payload_request.ssz");

    fn proof_request_body(proof_types: Vec<ProofType>) -> Vec<u8> {
        proof_request_body_with_fork(ProtocolFork::BPO2, proof_types)
    }

    fn proof_request_body_with_fork(fork: ProtocolFork, proof_types: Vec<ProofType>) -> Vec<u8> {
        let new_payload_request = NewPayloadRequest::from_ssz_bytes(NEW_PAYLOAD_REQUEST).unwrap();
        let chain_config = ChainConfig {
            chain_id: 1,
            active_fork: ForkConfig::new(ForkActivation::new(None, Some(0))),
        };
        ProofRequestBody {
            fork,
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
    async fn test_fork_invalid_payload_variant_returns_bad_request() {
        let body = proof_request_body_with_fork(ProtocolFork::Cancun, vec![ProofType::RethZisk]);
        assert_eq!(send(mock_app_state().await, body).await, 400);
    }

    #[tokio::test]
    async fn test_unknown_fork_returns_bad_request() {
        // `fork` is the leading fixed-size field, so overwriting the first byte with an
        // unassigned discriminant makes the body undecodable.
        let mut body = proof_request_body(vec![ProofType::RethZisk]);
        body[0] = 0xff;
        assert_eq!(send(mock_app_state().await, body).await, 400);
    }
}

//! Handler for `POST /v1/execution_proof_requests`.

use std::{collections::HashSet, sync::Arc};

use axum::{Json, extract::State};
use bytes::Bytes;
use tracing::{debug, info, info_span, instrument};
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

    // A clientless requester labels the fork from the Beacon API, which cannot
    // distinguish BPO forks that activate at the same epoch (their schedule
    // entries collapse into the effective one). Since the guest maps the label
    // to that fork's blob parameters, resolve the fork actually active on the
    // EL at the payload timestamp and correct compatible mislabels; a label
    // whose input schema disagrees with the EL's active fork is rejected. The
    // same chain-config response carries the EL's chain id, checked against
    // the request's first: a mismatch is rejected before proving resources
    // are spent.
    //
    // Only the label is rewritten. The submitted chain config is passed through
    // untouched because the guest echoes it into the proof's public values, and
    // the requester later reconstructs those values from its own copy when
    // verifying — the label itself is not part of the public values.
    let mut fork = request.fork;
    if let Some(cache) = &state.fork_schedule
        && let Some(schedule) = cache.get().await
    {
        if let Some(el_chain_id) = schedule.chain_id()
            && el_chain_id != request.chain_config.chain_id
        {
            return Err(ErrorResponse::bad_request(format!(
                "chain_config.chain_id {} does not match the execution layer's chain id {el_chain_id}",
                request.chain_config.chain_id
            )));
        }
        if let Some((el_fork, _)) = schedule.resolve(request.new_payload_request.timestamp())
            && el_fork != fork
        {
            if !is_valid_payload_variant(el_fork, &request.new_payload_request) {
                return Err(ErrorResponse::bad_request(format!(
                    "submitted fork {fork:?} does not match fork {el_fork:?} active on the execution layer at the payload timestamp"
                )));
            }
            // Info, not warn: on networks whose Beacon API collapses the
            // schedule the requester can never derive the right label, so
            // this fires on every request in steady state.
            info!(
                submitted = ?fork,
                resolved = ?el_fork,
                "normalized fork label to the execution layer's active fork"
            );
            fork = el_fork;
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
            fork,
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
///
/// Deliberately wildcard-free on the fork side: when the upstream [`ProtocolFork`] enum gains
/// a variant, this match stops compiling, forcing the schema assignment to be made explicitly.
fn is_valid_payload_variant(fork: ProtocolFork, new_payload_request: &NewPayloadRequest) -> bool {
    use NewPayloadRequest::*;
    use ProtocolFork::*;
    match fork {
        // Pre-merge forks have no execution payload schema.
        Frontier | Homestead | DAOFork | TangerineWhistle | SpuriousDragon | Byzantium
        | StPetersburg | Istanbul | MuirGlacier | Berlin | London | ArrowGlacier | GrayGlacier => {
            false
        }
        Paris => matches!(new_payload_request, Bellatrix(_)),
        Shanghai => matches!(new_payload_request, Capella(_)),
        Cancun => matches!(new_payload_request, Deneb(_)),
        Prague | Osaka | BPO1 | BPO2 => matches!(new_payload_request, ElectraFulu(_)),
        Amsterdam => matches!(new_payload_request, Gloas(_)),
    }
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

    use crate::{
        fork_schedule::{ElChainConfig, ForkSchedule, ForkScheduleCache},
        http::{
            AppState,
            tests::{mock_app_state, mock_app_state_with},
            v1::post_execution_proof_requests,
        },
        proof::ProofServiceMessage,
    };

    const NEW_PAYLOAD_REQUEST: &[u8] =
        include_bytes!("../../../tests/fixture/new_payload_request.ssz");

    fn proof_request_body(proof_types: Vec<ProofType>) -> Vec<u8> {
        proof_request_body_with_fork(ProtocolFork::BPO2, proof_types)
    }

    fn proof_request_body_with_fork(fork: ProtocolFork, proof_types: Vec<ProofType>) -> Vec<u8> {
        proof_request_body_with_activation(fork, 0, proof_types)
    }

    fn proof_request_body_with_activation(
        fork: ProtocolFork,
        activation_timestamp: u64,
        proof_types: Vec<ProofType>,
    ) -> Vec<u8> {
        let new_payload_request = NewPayloadRequest::from_ssz_bytes(NEW_PAYLOAD_REQUEST).unwrap();
        let chain_config = ChainConfig {
            chain_id: 1,
            active_fork: ForkConfig::new(ForkActivation::new(None, Some(activation_timestamp))),
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

    fn schedule_from(value: serde_json::Value) -> Arc<ForkScheduleCache> {
        let config: ElChainConfig = serde_json::from_value(value).unwrap();
        Arc::new(ForkScheduleCache::preset(ForkSchedule::new(&config)))
    }

    /// Devnet shape: BPO1 and BPO2 both activate at genesis, so a requester
    /// working from the Beacon API's collapsed blob schedule labels the
    /// active fork BPO1 while the EL's effective fork is BPO2. The chain id
    /// matches the test fixture's.
    fn devnet_schedule() -> Arc<ForkScheduleCache> {
        schedule_from(serde_json::json!({
            "chainId": 1,
            "shanghaiTime": 0,
            "cancunTime": 0,
            "pragueTime": 0,
            "osakaTime": 0,
            "bpo1Time": 0,
            "bpo2Time": 0,
        }))
    }

    #[tokio::test]
    async fn test_mislabeled_bpo_fork_is_normalized_against_el_schedule() {
        let (state, mut proof_service_rx) = mock_app_state_with(Some(devnet_schedule())).await;
        // The requester's own activation timestamp (7) deliberately differs
        // from the EL's (0): only the label may be rewritten, never the
        // chain config, which the guest echoes into the proof's public values
        // that the requester later verifies against.
        let body =
            proof_request_body_with_activation(ProtocolFork::BPO1, 7, vec![ProofType::RethZisk]);

        assert_eq!(send(state, body).await, 200);

        let message = proof_service_rx.recv().await.unwrap();
        let ProofServiceMessage::RequestProof {
            fork, chain_config, ..
        } = message
        else {
            panic!("expected a RequestProof message");
        };
        assert_eq!(fork, ProtocolFork::BPO2);
        assert_eq!(chain_config.chain_id, 1);
        assert_eq!(chain_config.active_fork.activation.timestamp(), Some(7));
    }

    #[tokio::test]
    async fn test_unknown_el_fork_time_disables_normalization() {
        // The EL schedules a fork this build does not model: resolution is
        // disabled, so the submitted label passes through unrewritten and the
        // request still succeeds.
        let schedule = schedule_from(serde_json::json!({
            "chainId": 1,
            "bpo1Time": 0,
            "bpo2Time": 0,
            "bpo3Time": 0,
        }));
        let (state, mut proof_service_rx) = mock_app_state_with(Some(schedule)).await;
        let body = proof_request_body_with_fork(ProtocolFork::BPO1, vec![ProofType::RethZisk]);

        assert_eq!(send(state, body).await, 200);

        let message = proof_service_rx.recv().await.unwrap();
        let ProofServiceMessage::RequestProof { fork, .. } = message else {
            panic!("expected a RequestProof message");
        };
        assert_eq!(fork, ProtocolFork::BPO1);
    }

    #[tokio::test]
    async fn test_unreachable_el_degrades_to_passthrough() {
        // A schedule cache whose EL fetch fails leaves normalization off; the
        // request succeeds with the submitted label.
        let cache = Arc::new(ForkScheduleCache::new(Arc::new(
            crate::el_client::ElClient::new(
                url::Url::parse("http://127.0.0.1:1/").unwrap(),
                reqwest::header::HeaderMap::new(),
            )
            .unwrap(),
        )));
        let (state, mut proof_service_rx) = mock_app_state_with(Some(cache)).await;
        let body = proof_request_body_with_fork(ProtocolFork::BPO1, vec![ProofType::RethZisk]);

        assert_eq!(send(state, body).await, 200);

        let message = proof_service_rx.recv().await.unwrap();
        let ProofServiceMessage::RequestProof { fork, .. } = message else {
            panic!("expected a RequestProof message");
        };
        assert_eq!(fork, ProtocolFork::BPO1);
    }

    #[tokio::test]
    async fn test_mismatched_chain_id_returns_bad_request() {
        let schedule = schedule_from(serde_json::json!({
            "chainId": 999,
            "bpo1Time": 0,
            "bpo2Time": 0,
        }));
        let (state, _proof_service_rx) = mock_app_state_with(Some(schedule)).await;
        let body = proof_request_body_with_fork(ProtocolFork::BPO2, vec![ProofType::RethZisk]);

        assert_eq!(send(state, body).await, 400);
    }

    #[tokio::test]
    async fn test_matching_fork_label_is_not_rewritten() {
        let (state, mut proof_service_rx) = mock_app_state_with(Some(devnet_schedule())).await;
        let body = proof_request_body_with_fork(ProtocolFork::BPO2, vec![ProofType::RethZisk]);

        assert_eq!(send(state, body).await, 200);

        let message = proof_service_rx.recv().await.unwrap();
        let ProofServiceMessage::RequestProof { fork, .. } = message else {
            panic!("expected a RequestProof message");
        };
        assert_eq!(fork, ProtocolFork::BPO2);
    }

    #[tokio::test]
    async fn test_fork_conflicting_with_el_schedule_returns_bad_request() {
        // The EL only schedules Cancun, whose payload schema differs from the
        // ElectraFulu payload submitted under a BPO2 label.
        let schedule = schedule_from(serde_json::json!({ "cancunTime": 0 }));
        let (state, _proof_service_rx) = mock_app_state_with(Some(schedule)).await;
        let body = proof_request_body_with_fork(ProtocolFork::BPO2, vec![ProofType::RethZisk]);

        assert_eq!(send(state, body).await, 400);
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

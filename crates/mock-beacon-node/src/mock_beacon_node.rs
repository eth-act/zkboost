//! State of the mock beacon node. It forwards every Engine API request of the CL to zkboost and
//! waits for the proof of every configured proof type of every valid `engine_newPayloadV5`
//! payload. It verifies the signature of every envelope as the beacon node does, then the proof
//! against the public values of the fixture proofs.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, bail, ensure};
use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{
        HeaderMap, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::post,
};
use ere_verifier::Verifier;
use lighthouse_bls::Signature;
use lighthouse_types::{BeaconBlockRef, ChainSpec, EthSpec, Hash256, MainnetEthSpec, Slot};
use serde::Deserialize;
use serde_json::{Value, json};
use stateless_validator_downloader::Downloader;
use tokio::{
    net::TcpListener,
    sync::{OnceCell, mpsc},
    time::{Instant, timeout_at},
};
use tracing::{info, warn};
use url::Url;
use zkboost_types::{
    MAX_EXECUTION_PROOFS_PER_PAYLOAD, MAX_PROOF_SIZE, NewPayloadParams, NewPayloadRequest,
    NewPayloadRequestExt, ProofType, SignedExecutionProofEnvelope, SignedExecutionProofEnvelopes,
    SszDecode, execution_proof_domain,
};

use crate::{beacon_node_client::BeaconNodeClient, engine_api_client::EngineApiClient};

const ERE_GUESTS_TAG: &str = "v0.17.0";

/// Budget for all proofs of one payload to arrive.
const PROOF_TIMEOUT: Duration = Duration::from_secs(60);

/// The SSZ size of a submission of `MAX_EXECUTION_PROOFS_PER_PAYLOAD` proofs of `MAX_PROOF_SIZE`.
/// Every envelope adds its list offset, the offset of its message, the offset of the proof data,
/// the proof type, the beacon block root, and the signature to the proof data.
const MAX_SUBMISSION_SIZE: usize =
    MAX_EXECUTION_PROOFS_PER_PAYLOAD * (4 + 4 + 4 + 1 + 32 + 96 + MAX_PROOF_SIZE);

/// The public values of the fixture proofs of the mock zkVM.
const MOCK_PUBLIC_VALUES: &[u8] =
    include_bytes!("../../server/src/proof/zkvm/mock/public_values.bin");

/// A valid payload of the CL, whose proofs are awaited.
#[derive(Clone)]
pub(crate) struct PendingPayload {
    slot: Slot,
    /// Receives the proof type of every verified proof.
    verified_tx: mpsc::UnboundedSender<u8>,
}

/// The mock beacon node, shared by the beacon API and the Engine API handlers.
pub(crate) struct MockBeaconNode {
    beacon_node_client: BeaconNodeClient,
    engine_api_client: EngineApiClient,
    spec: OnceCell<ChainSpec>,
    genesis_validators_root: OnceCell<Hash256>,
    verifiers: HashMap<u8, Verifier>,
    /// The valid payloads awaiting proofs, keyed by block hash.
    pending: Mutex<HashMap<Hash256, PendingPayload>>,
}

/// JSON-RPC request envelope, decoded to find `engine_newPayloadV5`.
#[derive(Deserialize)]
struct JsonRpcRequest {
    method: String,
    params: Value,
}

/// JSON-RPC response envelope of `engine_newPayloadV5`. Every other field is ignored.
#[derive(Deserialize)]
struct NewPayloadResponse {
    result: Option<PayloadStatus>,
}

/// Payload status of `engine_newPayloadV5`. Every other field is ignored.
#[derive(Deserialize)]
struct PayloadStatus {
    status: String,
}

impl MockBeaconNode {
    /// Builds the mock beacon node and downloads the verifier of every proof type. The chain
    /// spec and genesis validators root are fetched on first access.
    pub(crate) async fn new(
        cl_endpoint: Url,
        zkboost_endpoint: Url,
        proof_types: &[ProofType],
    ) -> anyhow::Result<Self> {
        let verifiers = download_vks_and_init(proof_types).await?;
        Ok(Self {
            beacon_node_client: BeaconNodeClient::new(cl_endpoint),
            engine_api_client: EngineApiClient::new(zkboost_endpoint),
            spec: OnceCell::new(),
            genesis_validators_root: OnceCell::new(),
            verifiers,
            pending: Default::default(),
        })
    }

    /// Serves the beacon API on the listener. It verifies the execution proofs and forwards every
    /// other request to the beacon node.
    pub(crate) async fn serve_beacon_api(
        self: Arc<Self>,
        listener: TcpListener,
    ) -> std::io::Result<()> {
        info!(address = %listener.local_addr()?, "beacon api listening");
        let router = Router::new()
            .route(
                "/eth/v1/beacon/execution_proofs",
                post(post_execution_proofs),
            )
            .route_layer(DefaultBodyLimit::max(MAX_SUBMISSION_SIZE))
            .fallback(forward_beacon_api_request)
            .with_state(self);
        axum::serve(listener, router).await
    }

    /// Serves the Engine API on the listener to the CL. It forwards every request to zkboost.
    pub(crate) async fn serve_engine_api(
        self: Arc<Self>,
        listener: TcpListener,
    ) -> std::io::Result<()> {
        info!(address = %listener.local_addr()?, "engine api listening");
        let router = Router::new()
            .route("/", post(forward_engine_api_request))
            .layer(DefaultBodyLimit::max(1 << 30))
            .with_state(self);
        axum::serve(listener, router).await
    }

    /// Returns the chain spec, fetching it on first access.
    pub(crate) async fn spec(&self) -> anyhow::Result<&ChainSpec> {
        self.spec
            .get_or_try_init(|| self.beacon_node_client.spec())
            .await
    }

    /// Returns the genesis validators root, fetching it on first access.
    pub(crate) async fn genesis_validators_root(&self) -> anyhow::Result<Hash256> {
        self.genesis_validators_root
            .get_or_try_init(|| self.beacon_node_client.genesis_validators_root())
            .await
            .copied()
    }

    /// Awaits the proofs of the payload of an `engine_newPayloadV5` request that zkboost
    /// answered with `VALID`. Every other request needs nothing.
    async fn new_payload(self: &Arc<Self>, request: &[u8], response: &[u8]) -> anyhow::Result<()> {
        let Ok(request) = serde_json::from_slice::<JsonRpcRequest>(request) else {
            return Ok(());
        };
        let Some(params) = NewPayloadParams::decode(&request.method, request.params) else {
            return Ok(());
        };
        let params = params.context("decode new payload params")?;
        let response: NewPayloadResponse =
            serde_json::from_slice(response).context("decode new payload response")?;
        let status = response.result.map(|result| result.status);
        if status.as_deref() != Some("VALID") {
            info!(block_hash = %params.block_hash(), ?status, "payload not valid");
            return Ok(());
        }
        self.await_proofs(params)
    }

    /// Registers a valid payload and waits in the background for the proof of every proof type.
    fn await_proofs(self: &Arc<Self>, params: NewPayloadParams) -> anyhow::Result<()> {
        let block_hash = Hash256::from(params.block_hash().0);
        let request = NewPayloadRequest::try_from(params).context("decode payload")?;
        let slot = Slot::new(request.slot().context("payload without slot")?);
        let (verified_tx, mut verified_rx) = mpsc::unbounded_channel();
        self.pending
            .lock()
            .unwrap()
            .insert(block_hash, PendingPayload { slot, verified_tx });
        info!(%block_hash, "valid payload forwarded");

        let mock_beacon_node = self.clone();
        tokio::spawn(async move {
            let mut remaining: HashSet<u8> = mock_beacon_node.verifiers.keys().copied().collect();
            let deadline = Instant::now() + PROOF_TIMEOUT;
            while !remaining.is_empty() {
                let Ok(Some(proof_type)) = timeout_at(deadline, verified_rx.recv()).await else {
                    warn!(%block_hash, ?remaining, "proofs not verified in time");
                    break;
                };
                remaining.remove(&proof_type);
            }
            mock_beacon_node.pending.lock().unwrap().remove(&block_hash);
            if remaining.is_empty() {
                info!(%block_hash, "all proofs verified");
            }
        });
        Ok(())
    }

    /// Returns the pending payload of the bid of a beacon block.
    async fn pending_payload(&self, block_root: Hash256) -> anyhow::Result<PendingPayload> {
        let block = self.beacon_node_client.block(block_root).await?;
        let BeaconBlockRef::Gloas(block) = block.message() else {
            bail!("block {block_root} is not a gloas block")
        };
        let block_hash = block.body.signed_execution_payload_bid.message.block_hash.0;
        self.pending
            .lock()
            .unwrap()
            .get(&block_hash)
            .cloned()
            .with_context(|| format!("payload {block_hash} not pending"))
    }

    /// Verifies the envelope as the beacon node does, then the proof, and returns the proof type.
    pub(crate) async fn verify(
        &self,
        pending: &PendingPayload,
        envelope: &SignedExecutionProofEnvelope,
    ) -> anyhow::Result<u8> {
        let proof_type = envelope.message.proof_type;
        self.verify_execution_proofs_signature(pending.slot, envelope)
            .await?;
        self.verify_execution_proof(proof_type, &envelope.message.proof_data)?;
        Ok(proof_type)
    }

    /// Verifies the proof with the verifier of its type and checks its public values.
    fn verify_execution_proof(&self, proof_type: u8, proof_data: &[u8]) -> anyhow::Result<()> {
        let Some(verifier) = self.verifiers.get(&proof_type) else {
            bail!("unsupported proof type {}", proof_type)
        };
        let public_values = verifier.verify(proof_data)?;

        // A zkVM with fixed size public values pads the SSZ result with zeros.
        let len = MOCK_PUBLIC_VALUES.len();
        ensure!(
            public_values.len() >= len
                && public_values[..len] == MOCK_PUBLIC_VALUES[..]
                && public_values[len..].iter().all(|byte| *byte == 0),
            "unexpected public values, expected {MOCK_PUBLIC_VALUES:?}, got: {public_values:?}"
        );
        Ok(())
    }

    async fn verify_execution_proofs_signature(
        &self,
        slot: Slot,
        envelope: &SignedExecutionProofEnvelope,
    ) -> anyhow::Result<()> {
        let signature = Signature::deserialize(&envelope.signature)
            .map_err(|err| anyhow::anyhow!("{err:?}"))?;
        let pubkey = self
            .beacon_node_client
            .validator_pubkey(envelope.validator_index)
            .await?;
        let epoch = slot.epoch(MainnetEthSpec::slots_per_epoch());
        let fork = self.spec().await?.fork_at_epoch(epoch);
        let domain =
            execution_proof_domain(fork.current_version, self.genesis_validators_root().await?);
        let signing_root = envelope.message.signing_root(domain);
        ensure!(
            signature.verify(&pubkey, signing_root),
            "invalid signature of validator {}",
            envelope.validator_index
        );
        Ok(())
    }
}

/// Handler for `POST /` of the Engine API. It answers with the response of zkboost to the
/// unchanged request. A valid `engine_newPayloadV5` payload then awaits its proofs. A request
/// that cannot reach zkboost is answered with 502.
async fn forward_engine_api_request(
    State(mock_beacon_node): State<Arc<MockBeaconNode>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (status, response) = match mock_beacon_node
        .engine_api_client
        .forward(headers.get(AUTHORIZATION), body.clone())
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            warn!(%error, "forward to zkboost failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    if let Err(error) = mock_beacon_node.new_payload(&body, &response).await {
        warn!(error = %format!("{error:#}"), "new payload not awaited");
    }
    (status, [(CONTENT_TYPE, "application/json")], response).into_response()
}

/// Handler for `POST /eth/v1/beacon/execution_proofs` with an SSZ body. A rejected envelope fails
/// the request with 400 and its index, as the beacon node answers.
async fn post_execution_proofs(
    State(mock_beacon_node): State<Arc<MockBeaconNode>>,
    body: Bytes,
) -> Response {
    let Ok(envelopes) = SignedExecutionProofEnvelopes::from_ssz_bytes(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut failures = Vec::new();
    for (index, envelope) in envelopes.iter().enumerate() {
        let block_root = Hash256::from(envelope.message.beacon_block_root);
        let result = async {
            let pending = mock_beacon_node.pending_payload(block_root).await?;
            let proof_type = mock_beacon_node.verify(&pending, envelope).await?;
            info!(%block_root, %proof_type, "proof verified");
            let _ = pending.verified_tx.send(proof_type);
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = result {
            warn!(%block_root, error = %format!("{error:#}"), "proof rejected");
            failures.push(json!({ "index": index, "message": format!("{error:#}") }));
        }
    }
    if failures.is_empty() {
        StatusCode::OK.into_response()
    } else {
        let body = json!({ "code": 400, "message": "error processing execution proofs", "failures": failures });
        (StatusCode::BAD_REQUEST, Json(body)).into_response()
    }
}

/// Handler for every other beacon API request, forwarded to the beacon node.
async fn forward_beacon_api_request(
    State(mock_beacon_node): State<Arc<MockBeaconNode>>,
    request: Request,
) -> Response {
    let method = request.method().clone();
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str().to_owned())
        .unwrap_or_default();
    let Ok(body) = to_bytes(request.into_body(), usize::MAX).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match mock_beacon_node
        .beacon_node_client
        .forward(method, &path_and_query, body)
        .await
    {
        Ok((status, Some(content_type), response)) => (
            status,
            [(CONTENT_TYPE, content_type)],
            Body::from_stream(response.bytes_stream()),
        )
            .into_response(),
        Ok((status, None, response)) => {
            (status, Body::from_stream(response.bytes_stream())).into_response()
        }
        Err(error) => {
            warn!(%path_and_query, %error, "forward to cl failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// Builds the verifier of every proof type from the verifying keys of the ere-guests release.
async fn download_vks_and_init(proof_types: &[ProofType]) -> anyhow::Result<HashMap<u8, Verifier>> {
    let downloader = Downloader::from_tag(ERE_GUESTS_TAG).await?;
    let mut verifiers = HashMap::new();
    for proof_type in proof_types {
        let stateless_validator = proof_type.stateless_validator_kind();
        let zkvm = proof_type.zkvm_kind().as_str().parse().unwrap();
        let guest = downloader.download(stateless_validator, zkvm).await?;
        let verifier = Verifier::new(proof_type.zkvm_kind(), &guest.program_vk)?;
        verifiers.insert(proof_type.execution_proof_type(), verifier);
        info!(%proof_type, "verifier loaded");
    }
    Ok(verifiers)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use url::Url;
    use zkboost_types::{MAX_PROOF_SIZE, ProofType};

    use crate::{
        beacon_node_client::BeaconNodeClient, engine_api_client::EngineApiClient,
        mock_beacon_node::MockBeaconNode,
    };

    /// Builds a node with no proof verifiers and endpoints where nothing listens.
    fn mock_beacon_node() -> MockBeaconNode {
        let endpoint = Url::parse("http://127.0.0.1:1").unwrap();
        MockBeaconNode {
            beacon_node_client: BeaconNodeClient::new(endpoint.clone()),
            engine_api_client: EngineApiClient::new(endpoint),
            spec: Default::default(),
            genesis_validators_root: Default::default(),
            verifiers: Default::default(),
            pending: Default::default(),
        }
    }

    /// A submission of the size of one maximum proof reaches the SSZ decoder, which rejects the
    /// zero bytes, instead of the body limit.
    #[tokio::test]
    async fn maximum_proof_size_submission_is_decoded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/eth/v1/beacon/execution_proofs",
            listener.local_addr().unwrap()
        );
        let beacon_api = tokio::spawn(async move {
            Arc::new(mock_beacon_node())
                .serve_beacon_api(listener)
                .await
                .unwrap();
        });
        let response = reqwest::Client::new()
            .post(endpoint)
            .body(vec![0; MAX_PROOF_SIZE])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        beacon_api.abort();
    }

    #[tokio::test]
    async fn test_fixture_proofs_verify() -> anyhow::Result<()> {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../server/src/proof/zkvm/mock");
        // The zesu guest of ere-guests v0.17.0 cannot be proved yet, so it has no fixture proof.
        let proof_types = ProofType::iter()
            .filter(|proof_type| *proof_type != ProofType::ZesuZisk)
            .collect::<Vec<_>>();
        let proofs = proof_types
            .iter()
            .map(|proof_type| {
                let stateless_validator = proof_type.stateless_validator_kind();
                let zkvm = proof_type.zkvm_kind();
                let zkvm_version = zkvm.sdk_version();
                let name = format!(
                    "stateless-validator-{stateless_validator}-{zkvm}-{zkvm_version}.proof",
                );
                fs::read(fixture.join(name)).unwrap()
            })
            .collect::<Vec<_>>();
        let dummy_url = Url::parse("x:").unwrap();
        let mock_beacon_node =
            MockBeaconNode::new(dummy_url.clone(), dummy_url, &proof_types).await?;
        for (proof_type, proof) in proof_types.into_iter().zip(&proofs) {
            let execution_proof_type = proof_type.execution_proof_type();
            mock_beacon_node.verify_execution_proof(execution_proof_type, proof)?;
        }
        Ok(())
    }
}

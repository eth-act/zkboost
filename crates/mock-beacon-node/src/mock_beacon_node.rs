//! State of the mock beacon node. It sends every Gloas payload to zkboost as
//! `engine_newPayloadV5` and waits for the proof of every configured proof type. It verifies the
//! signature of every envelope as the beacon node does, then the proof against the expected
//! public values.

use std::{
    collections::{HashMap, HashSet},
    net::Ipv4Addr,
    sync::Arc,
    time::Duration,
};

use alloy_rpc_types_engine::PayloadStatusEnum;
use anyhow::{bail, ensure};
use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::post,
};
use ere_verifier::Verifier;
use lighthouse_bls::Signature;
use lighthouse_types::{ChainSpec, EthSpec, ForkName, Hash256, MainnetEthSpec, Slot};
use serde_json::json;
use stateless_validator_downloader::Downloader;
use tokio::{
    net::TcpListener,
    sync::{Mutex, OnceCell, mpsc},
    time::{Instant, timeout_at},
};
use tracing::{info, warn};
use url::Url;
use zkboost_types::{
    HashTreeRoot, MOCK_PROOF, NewPayloadParams, NewPayloadRequest, ProofType, ProtocolFork,
    Sha2Hasher, SignedExecutionProofEnvelope, SignedExecutionProofEnvelopes, SszDecode, SszEncode,
    StatelessValidationResult, execution_proof_domain,
};

use crate::{
    beacon_node_client::{BeaconNodeClient, new_payload_request_gloas},
    engine_api_client::EngineApiClient,
};

const ERE_GUESTS_TAG: &str = "v0.17.0";

/// Budget for all proofs of one payload to arrive.
const PROOF_TIMEOUT: Duration = Duration::from_secs(60);

/// A payload sent to zkboost, with what its proofs are verified against.
#[derive(Clone)]
pub(crate) struct PendingPayload {
    slot: Slot,
    expected_public_values: Vec<u8>,
    /// Receives the proof type of every verified proof.
    pub(crate) verified_tx: mpsc::UnboundedSender<u8>,
}

/// The mock beacon node, shared by the block loop and the beacon API handlers.
pub(crate) struct MockBeaconNode {
    /// Client of the beacon API of the beacon node.
    pub(crate) beacon_node_client: BeaconNodeClient,
    engine_api_client: EngineApiClient,
    spec: OnceCell<ChainSpec>,
    genesis_validators_root: OnceCell<Hash256>,
    verifiers: HashMap<u8, Verifier>,
    /// Serializes execution updates; proof verification can continue in parallel.
    execution_update: Mutex<()>,
    /// The payloads awaiting proofs, keyed by beacon block root.
    pub(crate) pending: std::sync::Mutex<HashMap<Hash256, PendingPayload>>,
}

impl MockBeaconNode {
    /// Builds the mock beacon node with the static JWT secret and downloads the verifier of
    /// every proof type. The chain spec and genesis validators root are fetched on first access.
    pub(crate) async fn new(
        cl_endpoint: Url,
        zkboost_endpoint: Url,
        proof_types: &[ProofType],
    ) -> anyhow::Result<Self> {
        let beacon_node_client = BeaconNodeClient::new(cl_endpoint);
        let verifiers = download_vks_and_init(proof_types).await?;
        Ok(Self {
            beacon_node_client,
            engine_api_client: EngineApiClient::new(zkboost_endpoint)?,
            spec: OnceCell::new(),
            genesis_validators_root: OnceCell::new(),
            verifiers,
            execution_update: Mutex::new(()),
            pending: Default::default(),
        })
    }

    /// Serves the beacon API, verifying execution proofs and forwarding other requests to the
    /// beacon node.
    pub(crate) async fn serve(self: Arc<Self>, port: u16) -> std::io::Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await?;
        let router = Router::new()
            .route(
                "/eth/v1/beacon/execution_proofs",
                post(post_execution_proofs),
            )
            .fallback(forward)
            .with_state(self);
        info!(port, "beacon api listening");
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

    /// Reconciles execution with the upstream canonical head, then waits for its proofs.
    pub(crate) async fn process_head(&self) -> anyhow::Result<()> {
        // Coalesce notifications while a refresh is in flight. A periodic refresh also retries
        // SYNCING, recovers missed events, and handles a restarted execution node.
        let Ok(execution_update) = self.execution_update.try_lock() else {
            return Ok(());
        };
        let block_root = self.beacon_node_client.head_root().await?;
        let beacon_block = self.beacon_node_client.block(block_root).await?;
        let request = match beacon_block.fork_name_unchecked() {
            ForkName::Gloas => {
                let envelope = self
                    .beacon_node_client
                    .execution_payload_envelope(block_root)
                    .await?;
                NewPayloadRequest::Gloas(new_payload_request_gloas(&beacon_block, &envelope)?)
            }
            fork => {
                info!(%block_root, %fork, "fork not proven, skipped");
                return Ok(());
            }
        };

        let params = NewPayloadParams::try_from(&request)?;
        let block_hash = Hash256::from(params.block_hash().0);
        let parent_hash = Hash256::from(
            params
                .0
                .payload_inner
                .payload_inner
                .payload_inner
                .parent_hash
                .0,
        );
        let (safe_hash, finalized_hash) = self
            .beacon_node_client
            .finality_hashes(beacon_block.state_root())
            .await?;
        // Geth does not regenerate witnesses for known payloads. This also handles restarting
        // the mock or returning to an already executed block during a reorg.
        if self
            .engine_api_client
            .update_known_head(block_hash, safe_hash, finalized_hash)
            .await?
        {
            return Ok(());
        }

        // Announce and sync the parent first. Syncing to the new payload itself would import it
        // over P2P, so a subsequent newPayload call would return VALID without a witness.
        if !self
            .prepare_parent(beacon_block.parent_root(), parent_hash)
            .await?
        {
            info!(%block_root, %parent_hash, "execution parent syncing");
            return Ok(());
        }
        // REST reads and sync can take time. Never apply an older notification after a new head.
        if self.beacon_node_client.head_root().await? != block_root {
            return Ok(());
        }
        let new_payload_request_root = Hash256::from(request.hash_tree_root(&Sha2Hasher));
        let expected_public_values = StatelessValidationResult {
            new_payload_request_root: new_payload_request_root.0,
            successful_validation: true,
            chain_id: self.spec().await?.deposit_chain_id,
            schema_id: ProtocolFork::Amsterdam.schema_id(),
        }
        .to_ssz();

        let (verified_tx, mut verified_rx) = mpsc::unbounded_channel();
        self.pending.lock().unwrap().insert(
            block_root,
            PendingPayload {
                slot: beacon_block.slot(),
                expected_public_values,
                verified_tx,
            },
        );
        let result = async {
            let status = self.engine_api_client.new_payload(&params).await?;
            ensure!(
                status == PayloadStatusEnum::Valid,
                "new payload status {status}"
            );
            let status = self
                .engine_api_client
                .forkchoice_updated(block_hash, safe_hash, finalized_hash)
                .await?;
            ensure!(
                status == PayloadStatusEnum::Valid,
                "forkchoice status {status}"
            );
            drop(execution_update);
            info!(%block_root, %new_payload_request_root, %status, "new payload sent");

            let mut remaining: HashSet<u8> = self.verifiers.keys().copied().collect();
            let deadline = Instant::now() + PROOF_TIMEOUT;
            while !remaining.is_empty() {
                let Ok(Some(proof_type)) = timeout_at(deadline, verified_rx.recv()).await else {
                    bail!("proofs {remaining:?} not verified in time");
                };
                remaining.remove(&proof_type);
            }
            info!(%block_root, "all proofs verified");
            Ok(())
        }
        .await;
        self.pending.lock().unwrap().remove(&block_root);
        result
    }

    /// Registers an unknown parent header so forkchoice can trigger ordinary peer sync.
    async fn prepare_parent(
        &self,
        beacon_parent: Hash256,
        parent_hash: Hash256,
    ) -> anyhow::Result<bool> {
        if self
            .engine_api_client
            .forkchoice_updated(parent_hash, Hash256::ZERO, Hash256::ZERO)
            .await?
            == PayloadStatusEnum::Valid
        {
            return Ok(true);
        }
        let parent = self
            .beacon_node_client
            .parent_payload(beacon_parent, parent_hash)
            .await?;
        let status = self
            .engine_api_client
            .new_payload(&NewPayloadParams::try_from(&NewPayloadRequest::Gloas(
                parent,
            ))?)
            .await?;
        ensure!(
            matches!(
                status,
                PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted
            ),
            "parent payload status {status}"
        );
        Ok(self
            .engine_api_client
            .forkchoice_updated(parent_hash, Hash256::ZERO, Hash256::ZERO)
            .await?
            == PayloadStatusEnum::Valid)
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
        self.verify_execution_proof(
            proof_type,
            &envelope.message.proof_data,
            &pending.expected_public_values,
        )?;
        Ok(proof_type)
    }

    /// Verifies the proof with the verifier of its type and checks its public values.
    fn verify_execution_proof(
        &self,
        proof_type: u8,
        proof_data: &[u8],
        expected_public_values: &[u8],
    ) -> anyhow::Result<()> {
        let Some(verifier) = self.verifiers.get(&proof_type) else {
            bail!("unsupported proof type {}", proof_type)
        };

        if proof_data == MOCK_PROOF {
            return Ok(());
        }

        let public_values = verifier.verify(proof_data)?;

        // A zkVM with fixed size public values pads the SSZ result with zeros.
        let len = expected_public_values.len();
        anyhow::ensure!(
            public_values.len() >= len
                && public_values[..len] == expected_public_values[..]
                && public_values[len..].iter().all(|byte| *byte == 0),
            "unexpected public values, expected {expected_public_values:?}, got: {public_values:?}"
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
        let pending = mock_beacon_node
            .pending
            .lock()
            .unwrap()
            .get(&block_root)
            .cloned();
        let result = match pending {
            Some(pending) => mock_beacon_node
                .verify(&pending, envelope)
                .await
                .map(|proof_type| {
                    info!(%block_root, %proof_type, "proof verified");
                    let _ = pending.verified_tx.send(proof_type);
                }),
            None => Err(anyhow::anyhow!("unknown block")),
        };
        if let Err(error) = result {
            warn!(%block_root, %error, "proof rejected");
            failures.push(json!({ "index": index, "message": error.to_string() }));
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
async fn forward(
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
    use std::{fs, path::Path};

    use alloy_rpc_types_engine::PayloadStatusEnum;
    use url::Url;
    use zkboost_types::ProofType;

    use crate::mock_beacon_node::MockBeaconNode;

    /// Builds a node with a local Engine API stub and no proof verifiers.
    async fn engine_stub(
        results: Vec<serde_json::Value>,
    ) -> (
        MockBeaconNode,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{Json, Router, routing::post};
        let results = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            results,
        )));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let router = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let tx = tx.clone();
                let result = results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected Engine API request");
                async move {
                    let _ = tx.send(request);
                    Json(result)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let node = MockBeaconNode {
            beacon_node_client: crate::beacon_node_client::BeaconNodeClient::new(
                Url::parse("x:").unwrap(),
            ),
            engine_api_client: crate::engine_api_client::EngineApiClient::new(endpoint).unwrap(),
            spec: Default::default(),
            genesis_validators_root: Default::default(),
            verifiers: Default::default(),
            execution_update: Default::default(),
            pending: Default::default(),
        };
        (node, rx, server)
    }

    #[tokio::test]
    async fn forkchoice_carries_checkpoints_without_building_a_payload() {
        use lighthouse_types::Hash256;
        use serde_json::json;
        let (node, mut requests, server) = engine_stub(vec![
            json!({"result": {"payloadStatus": {"status": "SYNCING"}}}),
        ])
        .await;
        let head = Hash256::repeat_byte(3);
        let safe = Hash256::repeat_byte(2);
        let finalized = Hash256::repeat_byte(1);
        assert_eq!(
            node.engine_api_client
                .forkchoice_updated(head, safe, finalized)
                .await
                .unwrap(),
            PayloadStatusEnum::Syncing
        );
        let request = requests.recv().await.unwrap();
        assert_eq!(request["method"], "engine_forkchoiceUpdatedV4");
        let params = request["params"].as_array().unwrap();
        assert_eq!(params.len(), 3);
        assert_eq!(params[0]["headBlockHash"], json!(head));
        assert_eq!(params[0]["safeBlockHash"], json!(safe));
        assert_eq!(params[0]["finalizedBlockHash"], json!(finalized));
        assert!(params[1].is_null());
        assert!(params[2].is_null());
        server.abort();
    }

    #[tokio::test]
    async fn known_parent_does_not_need_a_payload_or_beacon_fetch() {
        use lighthouse_types::Hash256;
        use serde_json::json;
        let (node, mut requests, server) = engine_stub(vec![
            json!({"result": {"payloadStatus": {"status": "VALID"}}}),
        ])
        .await;
        let parent = Hash256::repeat_byte(1);
        assert!(
            node.prepare_parent(Hash256::repeat_byte(2), parent)
                .await
                .unwrap()
        );
        let request = requests.recv().await.unwrap();
        assert_eq!(request["params"][0]["headBlockHash"], json!(parent));
        assert!(requests.try_recv().is_err());
        server.abort();
    }

    #[tokio::test]
    async fn invalid_forkchoice_is_not_treated_as_syncing() {
        use lighthouse_types::Hash256;
        use serde_json::json;
        for response in [
            json!({"result": {"payloadStatus": {"status": "INVALID", "validationError": "bad ancestor"}}}),
            json!({"error": {"code": -38002, "message": "invalid forkchoice state"}}),
            json!({"result": {}}),
        ] {
            let (node, _, server) = engine_stub(vec![response]).await;
            assert!(
                node.prepare_parent(Hash256::ZERO, Hash256::repeat_byte(1))
                    .await
                    .is_err()
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn revisited_head_needs_valid_execution_state_but_no_new_payload() {
        use lighthouse_types::Hash256;
        use serde_json::json;
        let head = Hash256::repeat_byte(3);
        for status in ["VALID", "SYNCING"] {
            let (node, mut requests, server) = engine_stub(vec![
                json!({"result": {"hash": head}}),
                json!({"result": {"payloadStatus": {"status": status}}}),
            ])
            .await;
            assert_eq!(
                node.engine_api_client
                    .update_known_head(head, Hash256::ZERO, Hash256::ZERO)
                    .await
                    .unwrap(),
                status == "VALID"
            );
            assert_eq!(
                requests.recv().await.unwrap()["method"],
                "eth_getBlockByHash"
            );
            assert_eq!(
                requests.recv().await.unwrap()["method"],
                "engine_forkchoiceUpdatedV4"
            );
            assert!(requests.try_recv().is_err());
            server.abort();
        }
        let (node, mut requests, server) = engine_stub(vec![json!({"result": null})]).await;
        assert!(
            !node
                .engine_api_client
                .update_known_head(head, Hash256::ZERO, Hash256::ZERO)
                .await
                .unwrap()
        );
        assert_eq!(
            requests.recv().await.unwrap()["method"],
            "eth_getBlockByHash"
        );
        assert!(requests.try_recv().is_err());
        server.abort();
    }

    #[tokio::test]
    async fn test_fixture_proofs_verify() -> anyhow::Result<()> {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fixture");
        let public_values = fs::read(fixture.join("public_values.bin")).unwrap();
        let proof_types = ProofType::iter().collect::<Vec<_>>();
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
            mock_beacon_node.verify_execution_proof(execution_proof_type, proof, &public_values)?;
        }
        Ok(())
    }
}

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

use alloy_rpc_types_engine::{Claims, JwtSecret};
use anyhow::{Context, bail, ensure};
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
    HashTreeRoot, MOCK_PROOF, NewPayloadParams, NewPayloadRequest, ProofType, ProtocolFork,
    Sha2Hasher, SignedExecutionProofEnvelope, SignedExecutionProofEnvelopes, SszDecode, SszEncode,
    StatelessValidationResult, execution_proof_domain,
};

use crate::beacon_node_client::{BeaconNodeClient, new_payload_request_gloas};

const ERE_GUESTS_TAG: &str = "v0.17.0";

/// The static JWT secret of the ethereum-package, which every EL of a Kurtosis testnet accepts.
/// The mock beacon node only runs against such testnets.
const JWT_SECRET: &str = "0xdc49981516e8e72b401a63e6405495a32dafc3939b5d6d83cc319ac0388bca1b";

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
    zkboost_endpoint: Url,
    jwt_secret: JwtSecret,
    http: reqwest::Client,
    spec: OnceCell<ChainSpec>,
    genesis_validators_root: OnceCell<Hash256>,
    verifiers: HashMap<u8, Verifier>,
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
            zkboost_endpoint,
            jwt_secret: JwtSecret::from_hex(JWT_SECRET)?,
            http: reqwest::Client::new(),
            spec: OnceCell::new(),
            genesis_validators_root: OnceCell::new(),
            verifiers,
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

    /// Sends the payload of a Gloas block to zkboost and waits for the proof of every proof type.
    pub(crate) async fn process_block(&self, block_root: Hash256) -> anyhow::Result<()> {
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
            let status = self.new_payload(&params).await?;
            ensure!(status == "VALID", "new payload status {status}");
            info!(%block_root, %new_payload_request_root, status, "new payload sent");

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

    /// Sends the `engine_newPayload` call to zkboost and returns the payload status.
    async fn new_payload(&self, params: &NewPayloadParams) -> anyhow::Result<String> {
        let method = NewPayloadParams::METHOD;
        let token = self.jwt_secret.encode(&Claims::with_current_timestamp())?;
        let response: Value = self
            .http
            .post(self.zkboost_endpoint.clone())
            .bearer_auth(token)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(error) = response.get("error") {
            bail!("{method} failed: {error}");
        }
        response["result"]["status"]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("{method} response has no status"))
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
        let zkvm = proof_type.zkvm_kind();
        let guest = downloader.download(stateless_validator, zkvm).await?;
        let verifier = Verifier::new(zkvm, &guest.program_vk)?;
        verifiers.insert(proof_type.execution_proof_type(), verifier);
        info!(%proof_type, "verifier loaded");
    }
    Ok(verifiers)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use url::Url;
    use zkboost_types::ProofType;

    use crate::mock_beacon_node::MockBeaconNode;

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

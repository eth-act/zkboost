//! Mock beacon node. It mocks a beacon node with the EIP-8025 behavior. It follows the beacon
//! chain head of a CL, sends every Gloas payload to zkboost as `engine_newPayloadV5`, receives the
//! signed EIP-8025 envelopes at `POST /eth/v1/beacon/execution_proofs`, and verifies the signature
//! and the proof. Every other beacon API request goes to the CL.

#![warn(unused_crate_dependencies)]

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
use beacon_node_client::{BeaconNodeClient, new_payload_request_gloas};
use clap::Parser;
use ere_verifier::Verifier;
use jsonwebtoken as _;
use lighthouse_bls::Signature;
use lighthouse_types::{ChainSpec, EthSpec, ForkName, Hash256, MainnetEthSpec, Slot};
use serde_json::{Value, json};
use stateless_validator_downloader::Downloader;
use tokio::{
    net::TcpListener,
    sync::mpsc,
    time::{Instant, timeout_at},
};
use tokio_stream::StreamExt;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;
use zkboost_types::{
    HashTreeRoot, MOCK_PROOF, NewPayloadParams, NewPayloadRequest, ProofType, ProtocolFork,
    Sha2Hasher, SignedExecutionProofEnvelope, SignedExecutionProofEnvelopes, SszDecode, SszEncode,
    StatelessValidationResult, execution_proof_domain,
};

mod beacon_node_client;

const ERE_GUESTS_TAG: &str = "v0.17.0";

/// The static JWT secret of the ethereum-package, which every EL of a Kurtosis testnet accepts.
/// The mock beacon node only runs against such testnets.
const JWT_SECRET: &str = "0xdc49981516e8e72b401a63e6405495a32dafc3939b5d6d83cc319ac0388bca1b";

/// Budget for all proofs of one payload to arrive.
const PROOF_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Parser)]
struct Cli {
    /// Beacon API endpoint of the CL to follow.
    #[arg(long)]
    cl_endpoint: Url,
    /// Engine API endpoint of zkboost.
    #[arg(long)]
    zkboost_endpoint: Url,
    /// Proof types expected for every payload.
    #[arg(long, value_delimiter = ',')]
    proof_types: Vec<ProofType>,
    /// Port serving the beacon API.
    #[arg(long, default_value_t = 3001)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let beacon_node_client = BeaconNodeClient::new(cli.cl_endpoint);

    let spec = beacon_node_client.spec().await?;
    let genesis_validators_root = beacon_node_client.genesis_validators_root().await?;

    let mut verifiers = HashMap::new();
    let downloader = Downloader::from_tag(ERE_GUESTS_TAG).await?;
    for proof_type in &cli.proof_types {
        let stateless_validator_kind = proof_type.stateless_validator_kind();
        let zkvm_kind = proof_type.zkvm_kind();
        let program_vk = downloader
            .download(stateless_validator_kind, zkvm_kind)
            .await?
            .program_vk;
        let verifier = Verifier::new(zkvm_kind, &program_vk)?;
        verifiers.insert(proof_type.execution_proof_type(), verifier);
        info!(%proof_type, "verifier loaded");
    }

    let mock_beacon_node = Arc::new(MockBeaconNode {
        beacon_node_client,
        zkboost_endpoint: cli.zkboost_endpoint,
        jwt_secret: JwtSecret::from_hex(JWT_SECRET)?,
        http: reqwest::Client::new(),
        spec,
        genesis_validators_root,
        verifiers,
        pending: Default::default(),
    });

    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, cli.port)).await?;
    let router = Router::new()
        .route(
            "/eth/v1/beacon/execution_proofs",
            post(post_execution_proofs),
        )
        .fallback(forward_to_cl)
        .with_state(mock_beacon_node.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    info!(port = cli.port, "beacon api listening");

    let mut stream = mock_beacon_node.beacon_node_client.subscribe_blocks();
    while let Some(block) = stream.next().await {
        info!(slot = %block.slot, block = %block.block, "new block");
        let mock_beacon_node = mock_beacon_node.clone();
        tokio::spawn(async move {
            if let Err(error) = mock_beacon_node.process_block(block.block).await {
                warn!(slot = %block.slot, block = %block.block, error = %error, "block failed");
            }
        });
    }
    bail!("block stream ended")
}

struct MockBeaconNode {
    beacon_node_client: BeaconNodeClient,
    zkboost_endpoint: Url,
    jwt_secret: JwtSecret,
    http: reqwest::Client,
    spec: ChainSpec,
    genesis_validators_root: Hash256,
    verifiers: HashMap<u8, Verifier>,
    /// The payloads awaiting proofs, keyed by beacon block root.
    pending: std::sync::Mutex<HashMap<Hash256, PendingPayload>>,
}

/// A payload sent to zkboost, with what its proofs are verified against.
#[derive(Clone)]
struct PendingPayload {
    slot: Slot,
    expected_public_values: Vec<u8>,
    /// Receives the proof type of every verified proof.
    verified_tx: mpsc::UnboundedSender<u8>,
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

/// Handler for every other beacon API request, forwarded to the CL.
async fn forward_to_cl(
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

impl MockBeaconNode {
    async fn process_block(&self, block_root: Hash256) -> anyhow::Result<()> {
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
            chain_id: self.spec.deposit_chain_id,
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
    async fn verify(
        &self,
        pending: &PendingPayload,
        envelope: &SignedExecutionProofEnvelope,
    ) -> anyhow::Result<u8> {
        let proof_type = envelope.message.proof_type;
        let Some(verifier) = self.verifiers.get(&proof_type) else {
            bail!("unsupported proof type {}", proof_type)
        };

        self.verify_execution_proofs_signature(pending.slot, envelope)
            .await?;

        let proof_data = &*envelope.message.proof_data;
        if proof_data == MOCK_PROOF {
            return Ok(proof_type);
        }

        let public_values = verifier.verify(proof_data)?;

        // A zkVM with fixed size public values pads the SSZ result with zeros.
        let expected_public_values = &pending.expected_public_values;
        let len = expected_public_values.len();
        anyhow::ensure!(
            public_values.len() >= len
                && public_values[..len] == expected_public_values[..]
                && public_values[len..].iter().all(|byte| *byte == 0),
            "unexpected public values, expected {expected_public_values:?}, got: {public_values:?}"
        );

        Ok(proof_type)
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
        let fork = self.spec.fork_at_epoch(epoch);
        let domain = execution_proof_domain(fork.current_version, self.genesis_validators_root);
        let signing_root = envelope.message.signing_root(domain);
        ensure!(
            signature.verify(&pubkey, signing_root),
            "invalid signature of validator {}",
            envelope.validator_index
        );
        Ok(())
    }
}

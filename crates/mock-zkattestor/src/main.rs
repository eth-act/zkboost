//! Mock zkattestor.
//!
//! Follows the beacon chain head, sends every Gloas execution payload to the proof node as
//! `engine_newPayloadV5`, receives the
//! generated proofs at `POST /eth/v1/beacon/execution_proofs` as SSZ validator-signed EIP-8025
//! envelopes, verifies the signature against the validator of the CL, and verifies the proof
//! with `ere-verifier`. Every other beacon API request is forwarded to the CL, so the proof node
//! resolves beacon blocks through the attestor.

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
    body::{Bytes, to_bytes},
    extract::{Request, State},
    http::{StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::post,
};
use cl_client::{ClClient, new_payload_request_gloas};
use clap::Parser;
use ere_verifier::Verifier;
use futures::StreamExt;
// Selects the HMAC backend of the JWT encoder behind `alloy_rpc_types_engine::JwtSecret`.
use jsonwebtoken as _;
use lighthouse_bls::Signature;
use lighthouse_types::{ForkName, Hash256};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::mpsc};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;
use zkboost_types::{
    Hash256 as B256, HashTreeRoot, MOCK_PROOF, NewPayloadParams, NewPayloadRequest, ProofType,
    ProtocolFork, Sha2Hasher, SignedExecutionProofEnvelope, SignedExecutionProofEnvelopes,
    SszDecode, SszEncode, StatelessValidationResult, execution_proof_domain,
};

mod cl_client;

/// The static JWT secret of the ethereum-package, which every EL of a Kurtosis testnet accepts.
/// The attestor only runs against such testnets.
const JWT_SECRET: &str = "0xdc49981516e8e72b401a63e6405495a32dafc3939b5d6d83cc319ac0388bca1b";

/// Budget for all proofs of one payload to arrive.
const PROOF_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Parser)]
struct Cli {
    /// Beacon API endpoint of the CL to follow.
    #[arg(long)]
    cl_endpoint: Url,
    /// Engine API endpoint of the proof node.
    #[arg(long)]
    zkboost_endpoint: Url,
    /// Proof types expected for every payload.
    #[arg(long, value_delimiter = ',')]
    proof_types: Vec<ProofType>,
    /// Port serving the beacon API.
    #[arg(long, default_value_t = 3001)]
    port: u16,
    /// Program verifying key per proof type, as `<proof_type>=<path or URL>`. Proofs of a type
    /// without a verifying key are only accepted as the mock proof bytes.
    #[arg(long, value_parser = parse_program_vk)]
    program_vk: Vec<(ProofType, String)>,
}

fn parse_program_vk(value: &str) -> anyhow::Result<(ProofType, String)> {
    let (proof_type, source) = value
        .split_once('=')
        .context("expected <proof_type>=<path or URL>")?;
    Ok((proof_type.parse()?, source.to_owned()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let cl_client = ClClient::new(cli.cl_endpoint);

    let chain_id = cl_client.get_spec().await?.deposit_chain_id;
    let genesis_validators_root = cl_client.get_genesis_validators_root().await?;

    let mut verifiers = HashMap::new();
    for (proof_type, source) in cli.program_vk {
        let program_vk = load(&source).await?;
        let verifier = Verifier::new(proof_type.zkvm_kind(), &program_vk)
            .with_context(|| format!("init verifier for {proof_type}"))?;
        verifiers.insert(proof_type, verifier);
        info!(%proof_type, "verifier loaded");
    }

    let mock_attestor = Arc::new(MockAttestor {
        cl_client,
        zkboost_endpoint: cli.zkboost_endpoint,
        jwt_secret: JwtSecret::from_hex(JWT_SECRET)?,
        http: reqwest::Client::new(),
        proof_types: cli.proof_types,
        chain_id,
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
        .with_state(mock_attestor.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    info!(port = cli.port, "beacon api listening");

    let mut stream = Box::pin(mock_attestor.cl_client.subscribe_block_events());
    while let Some(Ok(block)) = stream.next().await {
        info!(slot = block.slot, block = %block.block, "new block");
        let mock_attestor = mock_attestor.clone();
        tokio::spawn(async move {
            if let Err(error) = mock_attestor.process_block(block.block).await {
                warn!(slot = block.slot, block = %block.block, error = %error, "block failed");
            }
        });
    }
    bail!("block stream ended")
}

/// Loads bytes from a local path or a remote URL.
async fn load(source: &str) -> anyhow::Result<Vec<u8>> {
    match Url::parse(source) {
        Ok(url) if url.scheme().starts_with("http") => Ok(reqwest::get(url)
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec()),
        _ => tokio::fs::read(source)
            .await
            .with_context(|| format!("read {source}")),
    }
}

struct MockAttestor {
    cl_client: ClClient,
    zkboost_endpoint: Url,
    jwt_secret: JwtSecret,
    http: reqwest::Client,
    proof_types: Vec<ProofType>,
    chain_id: u64,
    genesis_validators_root: Hash256,
    verifiers: HashMap<ProofType, Verifier>,
    /// The payloads awaiting proofs, keyed by beacon block root.
    pending: std::sync::Mutex<HashMap<Hash256, PendingPayload>>,
}

/// A payload sent to the proof node, with what its proofs are verified against.
#[derive(Clone)]
struct PendingPayload {
    state_root: Hash256,
    new_payload_request_root: Hash256,
    schema_id: u16,
    /// Receives the proof type of every verified proof.
    verified_tx: mpsc::UnboundedSender<ProofType>,
}

/// Handler for `POST /eth/v1/beacon/execution_proofs` with an SSZ body. Every envelope is
/// verified before the response, and a rejected envelope fails the request with 400 and its
/// index, as the beacon node answers.
async fn post_execution_proofs(
    State(mock_attestor): State<Arc<MockAttestor>>,
    body: Bytes,
) -> Response {
    let Ok(envelopes) = SignedExecutionProofEnvelopes::from_ssz_bytes(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut failures = Vec::new();
    for (index, envelope) in envelopes.iter().enumerate() {
        let block_root = Hash256::from(envelope.message.beacon_block_root);
        let pending = mock_attestor
            .pending
            .lock()
            .unwrap()
            .get(&block_root)
            .cloned();
        let verified = match pending {
            Some(pending) => mock_attestor
                .verify(&pending, envelope)
                .await
                .map(|proof_type| {
                    info!(%block_root, %proof_type, "proof verified");
                    let _ = pending.verified_tx.send(proof_type);
                }),
            None => Err(anyhow::anyhow!("unknown block")),
        };
        if let Err(error) = verified {
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
    State(mock_attestor): State<Arc<MockAttestor>>,
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
    match mock_attestor
        .cl_client
        .forward(method, &path_and_query, body)
        .await
    {
        Ok((status, Some(content_type), body)) => {
            (status, [(CONTENT_TYPE, content_type)], body).into_response()
        }
        Ok((status, None, body)) => (status, body).into_response(),
        Err(error) => {
            warn!(%path_and_query, %error, "forward to cl failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

impl MockAttestor {
    async fn process_block(&self, block_root: Hash256) -> anyhow::Result<()> {
        let beacon_block = self.cl_client.get_beacon_block(block_root).await?;
        let request = match beacon_block.fork_name_unchecked() {
            ForkName::Gloas => {
                let envelope = self
                    .cl_client
                    .get_execution_payload_envelope(block_root)
                    .await?;
                NewPayloadRequest::Gloas(new_payload_request_gloas(&beacon_block, &envelope)?)
            }
            fork => {
                info!(%block_root, %fork, "fork not proven, skipped");
                return Ok(());
            }
        };
        let state_root = beacon_block.state_root();
        let params = NewPayloadParams::try_from(&request)?;
        let block_hash = params.execution_payload_v1().block_hash;
        let schema_id = ProtocolFork::Amsterdam.schema_id();
        let new_payload_request_root = Hash256::from(request.hash_tree_root(&Sha2Hasher));

        let (verified_tx, mut verified_rx) = mpsc::unbounded_channel();
        self.pending.lock().unwrap().insert(
            block_root,
            PendingPayload {
                state_root,
                new_payload_request_root,
                schema_id,
                verified_tx,
            },
        );
        let result = async {
            let status = self.new_payload(&params).await?;
            info!(%block_root, %new_payload_request_root, %block_hash, status, "new payload sent");

            let mut remaining: HashSet<ProofType> = self.proof_types.iter().copied().collect();
            let deadline = tokio::time::Instant::now() + PROOF_TIMEOUT;
            while !remaining.is_empty() {
                let Ok(Some(proof_type)) =
                    tokio::time::timeout_at(deadline, verified_rx.recv()).await
                else {
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

    /// Sends the `engine_newPayload` call of the parameters to the proof node and returns the
    /// payload status.
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

    /// Verifies a submitted envelope as the beacon node does, then the proof against the
    /// expected public values, and returns the proof type.
    async fn verify(
        &self,
        pending: &PendingPayload,
        envelope: &SignedExecutionProofEnvelope,
    ) -> anyhow::Result<ProofType> {
        let proof_type = self
            .proof_types
            .iter()
            .copied()
            .find(|proof_type| proof_type.execution_proof_type() == envelope.message.proof_type)
            .with_context(|| format!("unsupported proof type {}", envelope.message.proof_type))?;
        ensure!(!envelope.message.proof_data.is_empty(), "empty proof data");
        let pubkey = self
            .cl_client
            .get_validator_pubkey(envelope.validator_index)
            .await?;
        let fork_version = self.cl_client.get_fork_version(pending.state_root).await?;
        let domain =
            execution_proof_domain(fork_version, B256::from(self.genesis_validators_root.0));
        let signing_root = envelope.message.signing_root(domain);
        let signature = Signature::deserialize(&envelope.signature)
            .map_err(|error| anyhow::anyhow!("signature: {error:?}"))?;
        ensure!(
            signature.verify(
                &pubkey,
                lighthouse_bls::Hash256::from_slice(signing_root.as_slice())
            ),
            "invalid signature of validator {}",
            envelope.validator_index
        );

        let proof_data: &[u8] = &envelope.message.proof_data;
        if proof_data == MOCK_PROOF {
            return Ok(proof_type);
        }
        let verifier = self
            .verifiers
            .get(&proof_type)
            .with_context(|| format!("no verifier for {proof_type}"))?;
        let public_values = verifier.verify(proof_data)?;

        let expected = StatelessValidationResult {
            new_payload_request_root: pending.new_payload_request_root.0,
            successful_validation: true,
            chain_id: self.chain_id,
            schema_id: pending.schema_id,
        }
        .to_ssz();
        let len = expected.len();

        // For zkVM with fixed size public values, ensure all padding after the
        // SSZ-encoded result are zeros.
        anyhow::ensure!(
            public_values.len() >= len
                && public_values[..len] == expected[..]
                && public_values[len..].iter().all(|byte| *byte == 0),
            "unexpected public values, expected {expected:?}, got: {public_values:?}"
        );
        Ok(proof_type)
    }
}

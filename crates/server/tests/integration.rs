//! Integration test for zkboost.

use std::{
    collections::HashMap,
    net::Ipv4Addr,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use alloy_genesis::ChainConfig;
use alloy_primitives::B256;
use axum::{Json, extract::State};
use bytes::Bytes;
use futures::StreamExt;
use metrics_exporter_prometheus::PrometheusBuilder;
use stateless::ExecutionWitness;
use tokio::net::TcpListener;
use zkboost_client::{MainnetEthSpec, zkBoostClient};
use zkboost_server::{
    config::{Config, zkVMConfig},
    server::zkBoostServer,
};
use zkboost_types::{
    Decode, FailureReason, Hash256, NewPayloadRequest, ProofEvent, ProofEventKind, ProofFailure,
    ProofStatus, ProofType, TreeHash,
};

struct Fixture {
    new_payload_request: NewPayloadRequest<MainnetEthSpec>,
    new_payload_request_root: Hash256,
    chain_config: ChainConfig,
    witness: ExecutionWitness,
}

impl Fixture {
    fn load() -> Self {
        const NEW_PAYLOAD_REQUEST: &[u8] = include_bytes!("fixture/new_payload_request.ssz");
        const CHAIN_CONFIG: &str = include_str!("fixture/chain_config.json");
        const EXECUTION_WITNESS: &str = include_str!("fixture/execution_witness.json");
        let new_payload_request = NewPayloadRequest::from_ssz_bytes(NEW_PAYLOAD_REQUEST).unwrap();
        let new_payload_request_root = new_payload_request.tree_hash_root();
        let chain_config: ChainConfig = serde_json::from_str(CHAIN_CONFIG).unwrap();
        let witness: ExecutionWitness = serde_json::from_str(EXECUTION_WITNESS).unwrap();
        Fixture {
            new_payload_request,
            new_payload_request_root,
            chain_config,
            witness,
        }
    }
}

async fn start_mock_el(fixture: &Fixture, witness_timeout: bool, witness_delay: bool) -> url::Url {
    struct MockElState {
        witnesses: HashMap<B256, ExecutionWitness>,
        chain_config: ChainConfig,
        witness_timeout: bool,
        witness_delay: bool,
        first_query_time: OnceLock<Instant>,
    }

    async fn mock_el_handler(
        State(state): State<Arc<MockElState>>,
        body: Bytes,
    ) -> Json<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let method = request["method"].as_str().unwrap_or("");

        let result = match method {
            "debug_chainConfig" => serde_json::to_value(&state.chain_config).unwrap(),
            "debug_executionWitnessByBlockHash" => {
                let hash_str = request["params"][0].as_str().unwrap();
                let hash: B256 = hash_str.parse().unwrap();

                if state.witness_timeout {
                    serde_json::Value::Null
                } else if state.witness_delay {
                    let first = state.first_query_time.get_or_init(Instant::now);
                    if first.elapsed() < Duration::from_secs(3) {
                        serde_json::Value::Null
                    } else {
                        state
                            .witnesses
                            .get(&hash)
                            .map(|w| serde_json::to_value(w).unwrap())
                            .unwrap_or(serde_json::Value::Null)
                    }
                } else {
                    state
                        .witnesses
                        .get(&hash)
                        .map(|w| serde_json::to_value(w).unwrap())
                        .unwrap_or(serde_json::Value::Null)
                }
            }
            _ => serde_json::Value::Null,
        };

        Json(serde_json::json!({
            "jsonrpc": "2.0",
            "result": result,
            "id": request["id"],
        }))
    }

    let block_hash = fixture.new_payload_request.block_hash();
    let witnesses = HashMap::from([(B256::from(block_hash.0), fixture.witness.clone())]);

    let state = Arc::new(MockElState {
        witnesses,
        chain_config: fixture.chain_config.clone(),
        witness_timeout,
        witness_delay,
        first_query_time: OnceLock::new(),
    });
    let app = axum::Router::new()
        .route("/", axum::routing::post(mock_el_handler))
        .with_state(state);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });

    format!("http://127.0.0.1:{port}").parse().unwrap()
}

async fn start_zkboost_server(
    el_endpoint: url::Url,
    zkvm_configs: Vec<zkVMConfig>,
    witness_timeout_secs: u64,
    proof_timeout_secs: u64,
) -> (url::Url, tokio_util::sync::CancellationToken) {
    let config = Config {
        port: 0,
        el_endpoint,
        chain_config_path: None,
        witness_timeout_secs,
        proof_timeout_secs,
        proof_cache_size: 128,
        witness_cache_size: 128,
        zkvm: zkvm_configs,
    };
    let metrics = PrometheusBuilder::new().build_recorder().handle();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let server = zkBoostServer::new(config, metrics).await.unwrap();
    let (addr, _) = server.run(shutdown.clone()).await.unwrap();
    let zkboost_endpoint = format!("http://127.0.0.1:{}", addr.port()).parse().unwrap();
    (zkboost_endpoint, shutdown)
}

#[derive(Default)]
struct Behavior {
    witness_delay: bool,
    witness_timeout: bool,
    proof_timeout: bool,
    proof_failure: bool,
}

struct TestHarness {
    fixture: Fixture,
    client: zkBoostClient,
    proof_type: ProofType,
    shutdown: tokio_util::sync::CancellationToken,
}

impl TestHarness {
    async fn new(behavior: Behavior) -> Self {
        let fixture = Fixture::load();
        let el_endpoint =
            start_mock_el(&fixture, behavior.witness_timeout, behavior.witness_delay).await;
        let proof_type = ProofType::EthrexZisk;
        let zkvm_config = zkVMConfig::Mock {
            proof_type,
            mock_proving_time: zkboost_server::config::MockProvingTime::Constant { ms: 3000 },
            mock_proof_size: 1024,
            mock_failure: behavior.proof_failure,
        };
        let witness_timeout_secs = if behavior.witness_timeout { 1 } else { 12 };
        let proof_timeout_secs = if behavior.proof_timeout { 1 } else { 12 };
        let (zkboost_endpoint, shutdown) = start_zkboost_server(
            el_endpoint,
            vec![zkvm_config],
            witness_timeout_secs,
            proof_timeout_secs,
        )
        .await;
        let client = zkBoostClient::new(zkboost_endpoint);
        Self {
            client,
            fixture,
            proof_type,
            shutdown,
        }
    }

    async fn request_proof(&self) {
        let new_payload_request_root = self
            .client
            .request_proof(&self.fixture.new_payload_request, &[self.proof_type])
            .await
            .unwrap()
            .new_payload_request_root;

        assert_eq!(
            new_payload_request_root,
            self.fixture.new_payload_request_root
        );
    }

    async fn wait_for_event(&self) -> ProofEvent {
        let mut stream = Box::pin(
            self.client
                .subscribe_proof_events(Some(self.fixture.new_payload_request_root)),
        );
        let proof_event = tokio::time::timeout(Duration::from_secs(30), async {
            stream.next().await.unwrap().unwrap()
        })
        .await
        .unwrap();

        assert_eq!(
            proof_event.new_payload_request_root(),
            self.fixture.new_payload_request_root
        );

        proof_event
    }

    async fn assert_proof_event(
        &self,
        proof_event_kind: ProofEventKind,
        failure_reason: Option<FailureReason>,
    ) {
        let proof_event = self.wait_for_event().await;

        assert_eq!(proof_event.kind(), proof_event_kind);
        assert_eq!(proof_event.proof_type(), self.proof_type);
        assert_eq!(
            proof_event.new_payload_request_root(),
            self.fixture.new_payload_request_root
        );
        if let Some(failure_reason) = failure_reason {
            assert!(matches!(
                proof_event,
                ProofEvent::ProofFailure(ProofFailure { reason, .. }) if reason == failure_reason
            ))
        }
    }

    async fn assert_proof_complete(&self) {
        self.assert_proof_event(ProofEventKind::ProofComplete, None)
            .await
    }

    async fn assert_proof_failure(&self, failure_reason: FailureReason) {
        self.assert_proof_event(ProofEventKind::ProofFailure, Some(failure_reason))
            .await
    }

    async fn assert_get_proof_is_valid(&self) {
        let proof = self
            .client
            .get_proof(self.fixture.new_payload_request_root, self.proof_type)
            .await
            .unwrap();

        let verification = self
            .client
            .verify_proof(
                self.fixture.new_payload_request_root,
                self.proof_type,
                &proof,
            )
            .await
            .unwrap();

        assert_eq!(verification.status, ProofStatus::Valid);
    }

    async fn assert_get_proof_not_found(&self) {
        assert!(matches!(
            self.client
                .get_proof(self.fixture.new_payload_request_root, self.proof_type)
                .await,
            Err(zkboost_client::Error::NotFound(_))
        ));
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[tokio::test]
async fn test_proof_complete() {
    let harness = TestHarness::new(Behavior::default()).await;

    harness.request_proof().await;
    harness.assert_proof_complete().await;
    harness.assert_get_proof_is_valid().await;

    harness.assert_proof_complete().await;
}

#[tokio::test]
async fn test_proof_complete_with_witness_delay() {
    let behavior = Behavior {
        witness_delay: true,
        ..Default::default()
    };
    let harness = TestHarness::new(behavior).await;

    harness.request_proof().await;
    harness.assert_proof_complete().await;
    harness.assert_get_proof_is_valid().await;
}

#[tokio::test]
async fn test_proof_failure() {
    let behavior = Behavior {
        proof_failure: true,
        ..Default::default()
    };
    let harness = TestHarness::new(behavior).await;

    harness.request_proof().await;
    harness
        .assert_proof_failure(FailureReason::ProvingError)
        .await;
    harness.assert_get_proof_not_found().await;
}

#[tokio::test]
async fn test_witness_timeout() {
    let behavior = Behavior {
        witness_timeout: true,
        ..Default::default()
    };
    let harness = TestHarness::new(behavior).await;

    harness.request_proof().await;
    harness
        .assert_proof_failure(FailureReason::WitnessTimeout)
        .await;
    harness.assert_get_proof_not_found().await;
}

#[tokio::test]
async fn test_proof_timeout() {
    let behavior = Behavior {
        proof_timeout: true,
        ..Default::default()
    };
    let harness = TestHarness::new(behavior).await;

    harness.request_proof().await;
    harness
        .assert_proof_failure(FailureReason::ProvingTimeout)
        .await;
    harness.assert_get_proof_not_found().await;
}

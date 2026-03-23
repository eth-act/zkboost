//! Reusable server initialization and startup.
//!
//! [`zkBoostServer::new`] performs async initialization (EL chain config fetch, zkVM
//! instance creation) and [`zkBoostServer::run`] binds the HTTP listener and spawns
//! all background services.

use std::{
    collections::HashMap,
    fs,
    net::{Ipv4Addr, SocketAddr},
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use alloy_genesis::ChainConfig;
use lru::LruCache;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::{
    net::TcpListener,
    sync::{RwLock, broadcast, mpsc},
    task::JoinHandle,
    time::sleep,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use zkboost_types::ProofType;

use crate::{
    config::Config,
    el_client::ElClient,
    http::{AppState, router},
    metrics::{set_build_info, set_programs_loaded},
    proof::{ProofService, worker, zkvm::zkVMInstance},
    witness::WitnessService,
};

const CHANNEL_CAPACITY: usize = 128;

/// Configured server ready to run.
#[allow(non_camel_case_types, missing_debug_implementations)]
pub struct zkBoostServer {
    el_client: Arc<ElClient>,
    chain_config: Arc<ChainConfig>,
    zkvms: Arc<HashMap<ProofType, zkVMInstance>>,
    config: Config,
    metrics: PrometheusHandle,
}

impl zkBoostServer {
    /// Creates a new server by initialising the EL client, fetching chain config,
    /// and creating zkVM instances from the given configuration.
    pub async fn new(config: Config, metrics: PrometheusHandle) -> anyhow::Result<Self> {
        info!(url = %config.el_endpoint, "el endpoint configured");
        let el_client = Arc::new(ElClient::new(config.el_endpoint.clone()));

        let chain_config = if let Some(path) = &config.chain_config_path {
            let content = fs::read_to_string(path)?;
            let chain_config: ChainConfig = serde_json::from_str(&content)?;
            info!("chain config loaded from file");
            chain_config
        } else {
            loop {
                match el_client.get_chain_config().await {
                    Ok(Some(chain_config)) => break chain_config,
                    Ok(None) => warn!(url = %el_client.url(), "chain config not available"),
                    Err(e) => {
                        warn!(url = %el_client.url(), error = %e, "chain config fetch failed")
                    }
                }
                info!("retrying chain config fetch");
                sleep(Duration::from_secs(2)).await;
            }
        };
        let chain_config = Arc::new(chain_config);
        info!("chain config loaded");

        let mut zkvms = HashMap::new();
        for zkvm_config in &config.zkvm {
            let instance = zkVMInstance::new(zkvm_config).await?;
            info!(
                proof_type = %zkvm_config.proof_type(),
                "zkvm instance created"
            );
            zkvms.insert(zkvm_config.proof_type(), instance);
        }
        set_programs_loaded(zkvms.len());
        set_build_info(env!("CARGO_PKG_VERSION"));

        Ok(Self {
            el_client,
            chain_config,
            zkvms: Arc::new(zkvms),
            config,
            metrics,
        })
    }

    /// Binds the HTTP listener, spawns background services, and returns the bound
    /// address with join handles.
    pub async fn run(
        self,
        shutdown_token: CancellationToken,
    ) -> anyhow::Result<(SocketAddr, Vec<JoinHandle<()>>)> {
        let witness_timeout = Duration::from_secs(self.config.witness_timeout_secs);
        let proof_timeout = Duration::from_secs(self.config.proof_timeout_secs);

        let completed_proofs = Arc::new(RwLock::new(LruCache::new(
            NonZeroUsize::new(self.config.proof_cache_size)
                .expect("proof_cache_size must be non-zero"),
        )));

        let (proof_service_tx, proof_service_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (witness_service_tx, witness_service_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (worker_output_tx, worker_output_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (proof_event_tx, proof_event_rx) = broadcast::channel(CHANNEL_CAPACITY);

        let mut handles = Vec::new();

        let witness_service = WitnessService::new(
            self.el_client,
            proof_service_tx.clone(),
            witness_timeout,
            self.config.witness_cache_size,
        );
        handles.push(witness_service.spawn(shutdown_token.clone(), witness_service_rx));

        info!("witness service started");

        let mut worker_input_txs = HashMap::new();
        for zkvm in self.zkvms.values() {
            let (worker_input_tx, worker_input_rx) = mpsc::channel(CHANNEL_CAPACITY);
            worker_input_txs.insert(zkvm.proof_type(), worker_input_tx);
            handles.push(tokio::spawn(worker::run_worker(
                zkvm.clone(),
                shutdown_token.clone(),
                worker_input_rx,
                worker_output_tx.clone(),
                proof_timeout,
            )));
        }

        let proof_service = ProofService::new(
            self.chain_config,
            completed_proofs.clone(),
            proof_event_tx,
            witness_service_tx,
            witness_timeout,
            proof_timeout,
        );
        handles.push(tokio::spawn(proof_service.run(
            shutdown_token.clone(),
            proof_service_rx,
            worker_output_rx,
            worker_input_txs,
        )));

        info!("proof service started");

        let app_state = Arc::new(AppState::new(
            self.zkvms.clone(),
            completed_proofs,
            proof_service_tx,
            proof_event_rx,
            self.metrics,
        ));
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, self.config.port)).await?;
        let addr = listener.local_addr()?;
        handles.push(tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router(app_state))
                .with_graceful_shutdown(shutdown_token.cancelled_owned())
                .await
            {
                error!(error = %error, "http server error");
            }
        }));

        info!(port = self.config.port, "http server listening");

        Ok((addr, handles))
    }
}

//! Reusable server initialization and startup.
//!
//! [`zkBoostServer::new`] performs async initialization (zkVM instance creation) and
//! [`zkBoostServer::run`] binds the Engine API listener and spawns all background services.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use metrics_exporter_prometheus::PrometheusHandle;
use tokio::{
    net::TcpListener,
    sync::{RwLock, broadcast, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use zkboost_types::ProofType;

use crate::{
    config::{Config, zkVMConfig},
    dashboard::{DashboardService, DashboardState},
    engine::EngineProxyState,
    http::{AppState, router},
    metrics::{set_build_info, set_programs_loaded},
    proof::{worker, zkvm::zkVMInstance},
};

const CHANNEL_CAPACITY: usize = 128;

/// Configured server ready to run.
#[allow(non_camel_case_types, missing_debug_implementations)]
pub struct zkBoostServer {
    zkvms: Arc<HashMap<ProofType, zkVMInstance>>,
    config: Config,
    metrics: PrometheusHandle,
}

impl zkBoostServer {
    /// Creates the zkVM instances of the configuration.
    pub async fn new(config: Config, metrics: PrometheusHandle) -> anyhow::Result<Self> {
        info!(url = %config.el_engine_endpoint, "el engine endpoint configured");
        info!(url = %config.cl_beacon_endpoint, "cl beacon endpoint configured");

        let mut zkvms = HashMap::new();
        for zkvm_config in &config.zkvm {
            let instance = zkVMInstance::new(zkvm_config).await?;
            let mode = match zkvm_config {
                zkVMConfig::Ere { .. } => "ere",
                zkVMConfig::Mock { .. } => "mock",
                zkVMConfig::Cluster { .. } => "cluster",
            };
            info!(
                proof_type = %zkvm_config.proof_type(),
                mode,
                "zkvm instance created"
            );
            zkvms.insert(zkvm_config.proof_type(), instance);
        }
        set_programs_loaded(zkvms.len());
        set_build_info(env!("CARGO_PKG_VERSION"));

        Ok(Self {
            zkvms: Arc::new(zkvms),
            config,
            metrics,
        })
    }

    /// Binds the Engine API listener, spawns background services, and returns the bound
    /// address with join handles.
    pub async fn run(
        self,
        shutdown_token: CancellationToken,
    ) -> anyhow::Result<(SocketAddr, Vec<JoinHandle<()>>)> {
        let (dashboard_service_tx, dashboard_service_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (worker_output_tx, worker_output_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (dashboard_event_tx, dashboard_event_rx) = broadcast::channel(CHANNEL_CAPACITY);

        let mut handles = Vec::new();

        let mut worker_input_txs = HashMap::new();
        for zkvm in self.zkvms.values() {
            let (worker_input_tx, worker_input_rx) = mpsc::channel(CHANNEL_CAPACITY);
            worker_input_txs.insert(zkvm.proof_type(), worker_input_tx);
            handles.push(tokio::spawn(worker::run_worker(
                zkvm.clone(),
                shutdown_token.clone(),
                worker_input_rx,
                worker_output_tx.clone(),
                dashboard_service_tx.clone(),
            )));
        }

        let engine = Arc::new(EngineProxyState::new(
            &self.config,
            worker_input_txs,
            dashboard_service_tx,
        )?);
        handles.push(tokio::spawn(
            engine
                .clone()
                .complete_proofs(shutdown_token.clone(), worker_output_rx),
        ));

        let dashboard = if self.config.dashboard.enabled {
            let dashboard = Arc::new(RwLock::new(DashboardState::new(
                self.zkvms.keys().copied(),
                self.config.dashboard.retention,
            )));

            let dashboard_service =
                DashboardService::new(dashboard.clone(), dashboard_event_tx.clone());
            handles.push(tokio::spawn(
                dashboard_service.run(shutdown_token.clone(), dashboard_service_rx),
            ));

            info!("dashboard service started");

            Some(dashboard)
        } else {
            drop(dashboard_service_rx);
            None
        };

        let app_state = Arc::new(AppState {
            engine,
            metrics: self.metrics,
            dashboard,
            dashboard_event_rx,
        });
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

        info!(port = self.config.port, "engine api listening");

        Ok((addr, handles))
    }
}

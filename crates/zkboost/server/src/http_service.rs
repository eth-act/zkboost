//! Application state, initialization, and HTTP endpoints.

use std::{collections::HashMap, iter::zip, sync::Arc};

use axum::{
    Router,
    extract::State,
    middleware,
    routing::{get, post},
};
use futures::future::try_join_all;
use metrics_exporter_prometheus::PrometheusHandle;
use reqwest::{Client, StatusCode};
use tokio::sync::mpsc::Sender;
use tower_http::trace::TraceLayer;
use zkboost_server_config::Config;
use zkboost_types::ProgramID;

use crate::{
    http_service::{
        execute::execute_program, info::get_server_info, prove::prove_program, verify::verify_proof,
    },
    metrics::http_metrics_middleware,
    proof_service::{ProofMessage, ProofService},
    zkvm::zkVMInstance,
};

mod execute;
mod info;
mod prove;
mod verify;

/// Application state shared across all HTTP handlers.
#[derive(Clone)]
pub(crate) struct AppState {
    /// Map of program IDs to their corresponding zkVM instances.
    pub(crate) programs: Arc<HashMap<ProgramID, zkVMInstance>>,
    /// Map of program IDs to proof service message sender.
    pub(crate) proof_txs: Arc<HashMap<ProgramID, Sender<ProofMessage>>>,
    /// Prometheus metrics handle for rendering metrics.
    pub(crate) metrics: PrometheusHandle,
}

impl AppState {
    /// Creates a new application state from configuration.
    ///
    /// Loads all configured zkVM programs and initializes their instances.
    pub(crate) async fn new(
        config: &Config,
        webhook_url: &str,
        metrics: PrometheusHandle,
    ) -> anyhow::Result<Self> {
        let zkvms = try_join_all(config.zkvm.iter().map(zkVMInstance::new)).await?;
        let http_client = Client::new();
        let mut proof_txs = HashMap::new();
        let mut programs = HashMap::new();
        for (zkvm_config, zkvm) in zip(&config.zkvm, zkvms) {
            let program_id = zkvm_config.program_id().clone();
            let (proof_service, proof_tx) = ProofService::new(
                program_id.clone(),
                zkvm.clone(),
                http_client.clone(),
                webhook_url.to_string(),
            );

            proof_service.start_service();

            proof_txs.insert(program_id.clone(), proof_tx);
            programs.insert(program_id, zkvm);
        }

        Ok(Self {
            programs: Arc::new(programs),
            proof_txs: Arc::new(proof_txs),
            metrics,
        })
    }
}

/// Builds the Axum router with all endpoints and middleware.
pub(crate) fn app(state: AppState) -> Router {
    Router::new()
        .route("/execute", post(execute_program))
        .route("/prove", post(prove_program))
        .route("/verify", post(verify_proof))
        .route("/info", get(get_server_info))
        .route("/health", get(StatusCode::OK))
        .route("/metrics", get(get_metrics))
        .with_state(state)
        .layer(middleware::from_fn(http_metrics_middleware))
        .layer(TraceLayer::new_for_http())
        // 400MB limit to account for the proof size
        // and the possibly large input size
        .layer(axum::extract::DefaultBodyLimit::max(400 * 1024 * 1024))
}

/// HTTP handler for the `/metrics` endpoint.
async fn get_metrics(State(state): State<AppState>) -> String {
    state.metrics.render()
}

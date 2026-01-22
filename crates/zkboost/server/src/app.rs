//! Application state, initialization, and HTTP endpoints.

use std::{collections::HashMap, iter::zip, sync::Arc};

use anyhow::Context;
use axum::{
    Router,
    extract::State,
    middleware,
    routing::{get, post},
};
use ere_dockerized::DockerizedzkVM;
use ere_server::client::zkVMClient;
use ere_zkvm_interface::{
    Input, ProgramExecutionReport, ProgramProvingReport, Proof, ProofKind, PublicValues,
};
use futures::future::try_join_all;
use metrics_exporter_prometheus::PrometheusHandle;
use reqwest::{StatusCode, Url};
use tower_http::trace::TraceLayer;
use zkboost_server_config::{Config, zkVMConfig};
use zkboost_types::ProgramID;

use crate::{
    app::{
        execute::execute_program, info::get_server_info, prove::prove_program, verify::verify_proof,
    },
    metrics::http_metrics_middleware,
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
    /// Prometheus metrics handle for rendering metrics.
    pub(crate) metrics: PrometheusHandle,
}

impl AppState {
    /// Creates a new application state from configuration.
    ///
    /// Loads all configured zkVM programs and initializes their instances.
    pub(crate) async fn new(config: &Config, metrics: PrometheusHandle) -> anyhow::Result<Self> {
        let zkvms = try_join_all(config.zkvm.iter().map(init_zkvm)).await?;
        let programs = zip(&config.zkvm, zkvms)
            .map(|(config, zkvm)| (config.program_id().clone(), zkvm))
            .collect();
        Ok(Self {
            programs: Arc::new(programs),
            metrics,
        })
    }
}

/// zkVM instance, either dockerized zkVM or external Ere server.
#[allow(non_camel_case_types)]
pub(crate) enum zkVMInstance {
    /// Dockerized zkVM managed by zkboost.
    Docker {
        /// The underlying zkVM implementation.
        vm: Arc<DockerizedzkVM>,
    },
    /// External Ere server that provides zkVM functionalities via http endpoints.
    External {
        /// Client of external Ere server.
        client: Arc<zkVMClient>,
    },
    /// Mock zkVM
    #[cfg(test)]
    Mock(crate::mock::MockzkVM),
}

impl zkVMInstance {
    /// Creates a dockerized zkVM instance.
    pub(crate) fn docker(vm: DockerizedzkVM) -> Self {
        Self::Docker { vm: Arc::new(vm) }
    }

    /// Creates an external zkVM instance.
    pub(crate) async fn external(endpoint: String) -> anyhow::Result<Self> {
        let endpoint = Url::parse(&endpoint)
            .with_context(|| format!("Failed to parse endpoint URL: {endpoint}"))?;
        let client = zkVMClient::from_endpoint(endpoint.clone())
            .with_context(|| format!("Failed to create zkVM client for endpoint: {endpoint}"))?;
        Ok(Self::External {
            client: Arc::new(client),
        })
    }

    /// Executes the program with the given input.
    pub(crate) async fn execute(
        &self,
        input: Input,
    ) -> anyhow::Result<(PublicValues, ProgramExecutionReport)> {
        match self {
            Self::Docker { vm } => vm.execute_async(input).await,
            Self::External { client } => Ok(client.execute(input).await?),
            #[cfg(test)]
            Self::Mock(vm) => vm.execute(&input),
        }
    }

    /// Creates a proof of the program execution with given input.
    pub(crate) async fn prove(
        &self,
        input: Input,
        proof_kind: ProofKind,
    ) -> anyhow::Result<(PublicValues, Proof, ProgramProvingReport)> {
        match self {
            Self::Docker { vm } => vm.prove_async(input, proof_kind).await,
            Self::External { client } => Ok(client.prove(input, proof_kind).await?),
            #[cfg(test)]
            Self::Mock(vm) => vm.prove(&input, proof_kind),
        }
    }

    /// Verifies a proof of the program used to create this zkVM instance, then
    /// returns the public values extracted from the proof.
    pub(crate) async fn verify(&self, proof: Proof) -> anyhow::Result<PublicValues> {
        match self {
            Self::Docker { vm } => vm.verify_async(proof).await,
            Self::External { client } => Ok(client.verify(proof).await?),
            #[cfg(test)]
            Self::Mock(vm) => vm.verify(&proof),
        }
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

/// Initializes a single zkVM instance from configuration.
async fn init_zkvm(config: &zkVMConfig) -> anyhow::Result<zkVMInstance> {
    match config {
        zkVMConfig::Docker {
            kind,
            resource,
            program,
            ..
        } => {
            let serialized_program = program.load().await?;
            let zkvm = DockerizedzkVM::new(*kind, serialized_program, resource.clone())
                .with_context(|| format!("Failed to initialize DockerizedzkVM, kind {kind}"))?;
            Ok(zkVMInstance::docker(zkvm))
        }
        zkVMConfig::External { endpoint, .. } => zkVMInstance::external(endpoint.clone()).await,
    }
}

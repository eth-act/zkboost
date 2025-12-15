//! Application state, initialization, and HTTP endpoints.

use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use axum::{
    Router,
    routing::{get, post},
};
use ere_dockerized::DockerizedzkVM;
use ere_zkvm_interface::zkVM;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use zkboost_types::ProgramID;

use crate::{
    app::{
        execute::execute_program, info::get_server_info, prove::prove_program, verify::verify_proof,
    },
    config::{Config, zkVMConfig},
};

mod execute;
mod info;
mod prove;
mod verify;

/// Application state shared across all HTTP handlers.
#[derive(Clone, Default)]
pub(crate) struct AppState {
    /// Map of program IDs to their corresponding zkVM instances.
    pub(crate) programs: Arc<RwLock<HashMap<ProgramID, zkVMInstance>>>,
}

impl AppState {
    /// Creates a new application state from configuration.
    ///
    /// Loads all configured zkVM programs and initializes their instances.
    pub(crate) fn new(config: &Config) -> anyhow::Result<Self> {
        let programs = config
            .zkvm
            .iter()
            .map(|zkvm_config| Ok((zkvm_config.program_id.clone(), init_zkvm(zkvm_config)?)))
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            programs: Arc::new(RwLock::new(programs)),
        })
    }
}

/// Wrapper around a zkVM instance that can be shared across threads.
#[derive(Clone)]
#[allow(non_camel_case_types)]
pub(crate) struct zkVMInstance {
    /// The underlying zkVM implementation.
    pub(crate) vm: Arc<dyn zkVM + Send + Sync>,
}

impl zkVMInstance {
    /// Creates a new zkVM instance from any type implementing the zkVM trait.
    pub(crate) fn new(vm: impl 'static + zkVM + Send + Sync) -> Self {
        Self { vm: Arc::new(vm) }
    }
}

/// Builds the Axum router with all endpoints and middleware.
pub(crate) fn app(state: AppState) -> Router {
    Router::new()
        .route("/execute", post(execute_program))
        .route("/prove", post(prove_program))
        .route("/verify", post(verify_proof))
        .route("/info", get(get_server_info))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        // 400MB limit to account for the proof size
        // and the possibly large input size
        .layer(axum::extract::DefaultBodyLimit::max(400 * 1024 * 1024))
}

/// Initializes a single zkVM instance from configuration.
fn init_zkvm(config: &zkVMConfig) -> anyhow::Result<zkVMInstance> {
    let zkvm = DockerizedzkVM::new(config.kind, config.program.load()?, config.resource.clone())
        .with_context(|| format!("Failed to initialize DockerizedzkVM, kind {}", config.kind))?;
    Ok(zkVMInstance::new(zkvm))
}

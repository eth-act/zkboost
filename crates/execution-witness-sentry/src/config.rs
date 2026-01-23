//! Configuration types for the execution witness sentry.

use std::path::Path;

use serde::{Deserialize, Serialize};
use url::Url;
use zkboost_ethereum_el_types::ElProofType;

use crate::error::{Error, Result};

/// Sentry configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Execution layer endpoints to monitor.
    #[serde(default)]
    pub el_endpoints: Vec<ElEndpoint>,
    /// Consensus layer endpoints to submit proofs to.
    #[serde(default)]
    pub cl_endpoints: Vec<ClEndpoint>,
    /// Directory to save block and witness data.
    pub output_dir: Option<String>,
    /// Chain identifier (used in output path).
    pub chain: Option<String>,
    /// Number of recent blocks to retain (older blocks are deleted).
    pub retain: Option<u64>,
    /// Number of proofs to submit per block.
    pub num_proofs: Option<u32>,
    /// Endpoint of proof engine.
    pub proof_engine: ProofEngineConfig,
}

/// Execution layer endpoint configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ElEndpoint {
    /// Human-readable name for this endpoint.
    pub name: String,
    /// HTTP JSON-RPC URL.
    pub url: Url,
    /// WebSocket URL for subscriptions.
    pub ws_url: Url,
}

/// Consensus layer endpoint configuration.
///
/// When the sentry starts if queries each CL endpoint to check whether its ENR
/// contains the zkVM flag, to determine whether the client requires proof
/// submission or not.
///
/// The first non-zkVM activated client will be used as the source of the new
/// head SSE subscription.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClEndpoint {
    /// Human-readable name for this endpoint.
    pub name: String,
    /// HTTP API URL.
    pub url: Url,
}

/// Configuration for the proof engine.
///
/// The proof engine receives proof requests and asynchronously generates
/// proofs, pushing results back via a webhook. Multiple proof types can
/// be configured to generate different proof variants per block.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProofEngineConfig {
    /// Proof engine URL.
    pub url: Url,
    /// Proof types the proof engine supports.
    pub proof_types: Vec<ElProofType>,
    /// Port for HTTP server to receive proofs from proof engine.
    #[serde(default = "default_proof_engine_webhook_port")]
    pub webhook_port: u16,
}

/// Returns the default webhook port for receiving proofs from the proof engine.
fn default_proof_engine_webhook_port() -> u16 {
    3003
}

impl Config {
    /// Load configuration from a TOML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            Error::Config(format!(
                "failed to read config file '{}': {}",
                path.as_ref().display(),
                e
            ))
        })?;
        Ok(toml_edit::de::from_str(&content)?)
    }
}

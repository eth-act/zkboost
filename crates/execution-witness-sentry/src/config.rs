//! Configuration types for the execution witness sentry.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Sentry configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Execution layer endpoints to monitor.
    pub endpoints: Vec<Endpoint>,
    /// Directory to save block and witness data.
    pub output_dir: Option<String>,
    /// Chain identifier (used in output path).
    pub chain: Option<String>,
    /// Number of recent blocks to retain (older blocks are deleted).
    pub retain: Option<u64>,
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
        Ok(toml::from_str(&content)?)
    }
}

/// Execution layer endpoint configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Endpoint {
    /// Human-readable name for this endpoint.
    pub name: String,
    /// HTTP JSON-RPC URL.
    pub el_url: String,
    /// WebSocket URL for subscriptions.
    pub el_ws_url: String,
}

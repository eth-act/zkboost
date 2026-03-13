//! Configuration types.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use url::Url;
use zkboost_types::ProofType;

const DEFAULT_PORT: u16 = 3000;
const DEFAULT_WITNESS_TIMEOUT_SECS: u64 = 12;
const DEFAULT_PROOF_TIMEOUT_SECS: u64 = 12;
const DEFAULT_PROOF_CACHE_SIZE: usize = 128;
const DEFAULT_WITNESS_CACHE_SIZE: usize = 128;

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_witness_timeout_secs() -> u64 {
    DEFAULT_WITNESS_TIMEOUT_SECS
}

fn default_proof_timeout_secs() -> u64 {
    DEFAULT_PROOF_TIMEOUT_SECS
}

fn default_proof_cache_size() -> usize {
    DEFAULT_PROOF_CACHE_SIZE
}

fn default_witness_cache_size() -> usize {
    DEFAULT_WITNESS_CACHE_SIZE
}

/// Unified configuration for the zkboost proof node.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// HTTP server port.
    #[serde(default = "default_port")]
    pub port: u16,
    /// EL endpoint for witness fetching.
    pub el_endpoint: Url,
    /// Optional path to a local chain config JSON file.
    #[serde(default)]
    pub chain_config_path: Option<PathBuf>,
    /// Timeout in seconds for witness data (both pending-proof and fetch staleness).
    #[serde(default = "default_witness_timeout_secs")]
    pub witness_timeout_secs: u64,
    /// Timeout in seconds for proof generation per zkVM worker.
    #[serde(default = "default_proof_timeout_secs")]
    pub proof_timeout_secs: u64,
    /// Maximum number of completed proofs to keep in the LRU cache.
    #[serde(default = "default_proof_cache_size")]
    pub proof_cache_size: usize,
    /// Maximum number of execution witnesses to keep in the LRU cache.
    #[serde(default = "default_witness_cache_size")]
    pub witness_cache_size: usize,
    /// zkVM backend configurations.
    pub zkvm: Vec<zkVMConfig>,
}

/// zkVM backend configuration, either a remote ere-server or a mock for testing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum zkVMConfig {
    /// Remote ere-server backend.
    External {
        /// HTTP endpoint URL of the ere-server.
        endpoint: String,
        /// Proof type.
        proof_type: ProofType,
    },
    /// In-process mock backend for testing.
    Mock {
        /// Proof type.
        proof_type: ProofType,
        /// Simulated proving latency in milliseconds.
        mock_proving_time_ms: u64,
        /// Size of the mock proof in bytes.
        mock_proof_size: u64,
        /// Whether the mock should always fail proof generation.
        #[serde(default)]
        mock_failure: bool,
    },
}

impl zkVMConfig {
    /// Returns the proof type identifier for this configuration.
    pub fn proof_type(&self) -> ProofType {
        match self {
            Self::External { proof_type, .. } | Self::Mock { proof_type, .. } => *proof_type,
        }
    }
}

impl Config {
    /// Load configuration from a TOML file at the given path.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = fs::read_to_string(path.as_ref())?;
        Ok(toml_edit::de::from_str(&content)?)
    }
}

#[cfg(test)]
mod tests {
    use zkboost_types::ProofType;

    use crate::config::{Config, zkVMConfig};

    #[test]
    fn test_parse_multiple_zkvms() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"

            [[zkvm]]
            kind = "external"
            endpoint = "http://ere-server:3000"
            proof_type = "ethrex-zisk"

            [[zkvm]]
            kind = "mock"
            proof_type = "reth-zisk"
            mock_proving_time_ms = 100
            mock_proof_size = 512
        "#;

        let config: Config = toml_edit::de::from_str(toml).unwrap();

        assert_eq!(config.zkvm.len(), 2);
        assert_eq!(config.zkvm[0].proof_type(), ProofType::EthrexZisk);
        assert_eq!(config.zkvm[1].proof_type(), ProofType::RethZisk);

        assert!(matches!(&config.zkvm[0], zkVMConfig::External { .. }));
        assert!(matches!(&config.zkvm[1], zkVMConfig::Mock { .. }));
    }

    #[test]
    fn test_cache_size_defaults() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
            mock_proving_time_ms = 10
            mock_proof_size = 64
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert_eq!(config.proof_cache_size, 128);
        assert_eq!(config.witness_cache_size, 128);
    }

    #[test]
    fn test_cache_size_overrides() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            proof_cache_size = 256
            witness_cache_size = 64
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
            mock_proving_time_ms = 10
            mock_proof_size = 64
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert_eq!(config.proof_cache_size, 256);
        assert_eq!(config.witness_cache_size, 64);
    }
}

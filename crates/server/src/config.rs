//! TOML configuration of the proof node, with validation of the zkVM entries.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, ensure};
use ere_verifier::zkVMKind;
use serde::{Deserialize, Serialize};
use url::Url;
use zkboost_types::ProofType;

const DEFAULT_PORT: u16 = 3000;
const DEFAULT_PROOF_TIMEOUT_SECS: u64 = 12;
const DEFAULT_DASHBOARD_ENABLED: bool = false;
const DEFAULT_DASHBOARD_RETENTION: usize = 256;

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_proof_timeout_secs() -> u64 {
    DEFAULT_PROOF_TIMEOUT_SECS
}

fn default_mock_proving_time() -> MockProvingTime {
    MockProvingTime::Constant { ms: 6000 }
}

fn default_dashboard_enabled() -> bool {
    DEFAULT_DASHBOARD_ENABLED
}

fn default_dashboard_retention() -> usize {
    DEFAULT_DASHBOARD_RETENTION
}

/// Unified configuration for the zkboost proof node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Engine API server port.
    #[serde(default = "default_port")]
    pub port: u16,
    /// EL Engine API endpoint that every Engine API request is forwarded to.
    pub el_engine_endpoint: Url,
    /// Beacon API endpoint that receives every proof at `POST /eth/v1/beacon/execution_proofs`.
    pub cl_beacon_endpoint: Url,
    /// EIP-2335 keystore of the validator that signs the proofs.
    pub validator_keystore_path: PathBuf,
    /// Plain text password of the keystore.
    pub validator_keystore_password_path: PathBuf,
    /// Dashboard feature configuration.
    #[serde(default)]
    pub dashboard: DashboardConfig,
    /// zkVM backend configurations.
    pub zkvm: Vec<zkVMConfig>,
}

impl Config {
    /// Loads configuration from a TOML file at the given path.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = fs::read_to_string(path.as_ref())?;
        let config: Self = toml_edit::de::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.zkvm.is_empty(),
            "at least one [[zkvm]] entry is required"
        );
        ensure!(
            self.dashboard.retention > 0,
            "dashboard.retention must be > 0"
        );
        let mut proof_types = HashSet::new();
        for zkvm in &self.zkvm {
            let proof_type = zkvm.proof_type();
            ensure!(
                proof_types.insert(proof_type),
                "duplicate proof_type: {proof_type}"
            );
            match zkvm {
                zkVMConfig::Ere {
                    proof_timeout_secs, ..
                }
                | zkVMConfig::Mock {
                    proof_timeout_secs, ..
                }
                | zkVMConfig::Cluster {
                    proof_timeout_secs, ..
                } => {
                    ensure!(
                        *proof_timeout_secs > 0,
                        "proof_timeout_secs must be > 0 for {proof_type}"
                    );
                }
            }
            if let zkVMConfig::Mock {
                mock_proving_time: MockProvingTime::Random { min_ms, max_ms },
                ..
            } = zkvm
            {
                ensure!(
                    min_ms <= max_ms,
                    "mock_proving_time random: min_ms ({min_ms}) must be <= max_ms ({max_ms})"
                );
            }
            if let zkVMConfig::Cluster {
                proof_type,
                elf_path,
                elf_url,
                ..
            } = zkvm
            {
                ensure!(
                    matches!(proof_type.zkvm_kind(), zkVMKind::Zisk),
                    "proof_type {proof_type} is not supported by cluster backend"
                );
                ensure!(
                    elf_path.is_some() || elf_url.is_some(),
                    "cluster zkvm {proof_type}: one of elf_path or elf_url must be set"
                );
                ensure!(
                    !(elf_path.is_some() && elf_url.is_some()),
                    "cluster zkvm {proof_type}: elf_path and elf_url are mutually exclusive"
                );
            }
        }
        Ok(())
    }
}

/// Mock proving time configuration, supporting constant, random, and gas-proportional modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MockProvingTime {
    /// Fixed proving time.
    Constant {
        /// Proving time in milliseconds.
        ms: u64,
    },
    /// Random proving time uniformly sampled from [min_ms, max_ms].
    Random {
        /// Minimum proving time in milliseconds.
        min_ms: u64,
        /// Maximum proving time in milliseconds.
        max_ms: u64,
    },
    /// Proving time proportional to block gas usage.
    Linear {
        /// Milliseconds per million gas used.
        ms_per_mgas: u64,
    },
}

/// zkVM backend configuration. One of a remote ere-server, a mock, or an external proving
/// cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum zkVMConfig {
    /// Remote ere-server backend.
    Ere {
        /// Proof type.
        proof_type: ProofType,
        /// Timeout in seconds for proof generation.
        #[serde(default = "default_proof_timeout_secs")]
        proof_timeout_secs: u64,
        /// HTTP endpoint URL of the ere-server.
        endpoint: Url,
    },
    /// In-process mock backend for testing.
    Mock {
        /// Proof type.
        proof_type: ProofType,
        /// Timeout in seconds for proof generation.
        #[serde(default = "default_proof_timeout_secs")]
        proof_timeout_secs: u64,
        /// Simulated proving time configuration.
        #[serde(default = "default_mock_proving_time")]
        mock_proving_time: MockProvingTime,
        /// Whether the mock should always fail proof generation.
        #[serde(default)]
        mock_failure: bool,
    },
    /// Remote cluster backend.
    Cluster {
        /// Proof type. Currently only supports ZisK proof types.
        proof_type: ProofType,
        /// Timeout in seconds for proof generation.
        #[serde(default = "default_proof_timeout_secs")]
        proof_timeout_secs: u64,
        /// Endpoint of the cluster.
        endpoint: String,
        /// Optional local file path to the ELF. Mutually exclusive with `elf_url`.
        #[serde(default)]
        elf_path: Option<PathBuf>,
        /// Optional URL to fetch the ELF from. Mutually exclusive with `elf_path`.
        #[serde(default)]
        elf_url: Option<String>,
    },
}

impl zkVMConfig {
    /// Returns the proof type identifier for this configuration.
    pub fn proof_type(&self) -> ProofType {
        match self {
            Self::Ere { proof_type, .. }
            | Self::Mock { proof_type, .. }
            | Self::Cluster { proof_type, .. } => *proof_type,
        }
    }
}

/// Loads asset bytes from a local path or a remote URL.
///
/// Exactly one of `path` or `url` is expected to be set, enforced by config
/// validation. A set `path` takes precedence.
pub(crate) async fn load(path: &Option<PathBuf>, url: &Option<String>) -> anyhow::Result<Vec<u8>> {
    /// Bounds a remote asset fetch so a slow or unresponsive host cannot hang
    /// startup indefinitely.
    const ASSET_FETCH_TIMEOUT: Duration = Duration::from_secs(120);

    match (path, url) {
        (Some(path), _) => fs::read(path).with_context(|| format!("read from {}", path.display())),
        (_, Some(url)) => {
            let bytes = reqwest::Client::builder()
                .timeout(ASSET_FETCH_TIMEOUT)
                .build()?
                .get(url)
                .send()
                .await
                .with_context(|| format!("GET {url}"))?
                .error_for_status()
                .with_context(|| format!("status from {url}"))?
                .bytes()
                .await
                .with_context(|| format!("body from {url}"))?
                .to_vec();
            Ok(bytes)
        }
        (None, None) => unreachable!("config validation requires a path or a URL"),
    }
}

/// Dashboard feature configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    /// Whether the live dashboard UI and API endpoints are enabled.
    #[serde(default = "default_dashboard_enabled")]
    pub enabled: bool,
    /// Maximum number of recent block records to keep in the dashboard history.
    #[serde(default = "default_dashboard_retention")]
    pub retention: usize,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: default_dashboard_enabled(),
            retention: default_dashboard_retention(),
        }
    }
}

#[cfg(test)]
mod tests {
    use zkboost_types::ProofType;

    use crate::config::{Config, MockProvingTime, zkVMConfig};

    /// Parses a config with the shared top-level keys and the given zkVM entries.
    fn parse(zkvm: &str) -> Config {
        let toml = format!(
            r#"
            el_engine_endpoint = "http://localhost:8551"
            cl_beacon_endpoint = "http://localhost:4000"
            validator_keystore_path = "/validator-keys/keys/0xaa/voting-keystore.json"
            validator_keystore_password_path = "/validator-keys/secrets/0xaa"
            {zkvm}
        "#
        );
        toml_edit::de::from_str(&toml).unwrap()
    }

    #[test]
    fn test_parse_multiple_zkvms() {
        let toml = r#"
            [[zkvm]]
            kind = "ere"
            endpoint = "http://ere-server:3000"
            proof_type = "ethrex-zisk"

            [[zkvm]]
            kind = "mock"
            proof_type = "reth-zisk"
            mock_proving_time = { kind = "constant", ms = 100 }
        "#;

        let config = parse(toml);

        assert_eq!(config.zkvm.len(), 2);
        assert_eq!(config.zkvm[0].proof_type(), ProofType::EthrexZisk);
        assert_eq!(config.zkvm[1].proof_type(), ProofType::RethZisk);

        assert!(matches!(&config.zkvm[0], zkVMConfig::Ere { .. }));
        assert!(matches!(&config.zkvm[1], zkVMConfig::Mock { .. }));
    }

    #[test]
    fn test_defaults() {
        let toml = r#"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
        "#;
        let config = parse(toml);
        assert!(matches!(
            config.zkvm[0],
            zkVMConfig::Mock {
                proof_timeout_secs: 12,
                mock_proving_time: MockProvingTime::Constant { ms: 6000 },
                ..
            }
        ));
    }

    #[test]
    fn test_empty_zkvm_rejected() {
        let toml = r#"
            zkvm = []
        "#;
        let config = parse(toml);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_zero_dashboard_retention_rejected() {
        let toml = r#"
            [dashboard]
            enabled = true
            retention = 0
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
        "#;
        let config = parse(toml);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_random_proving_time_min_gt_max_rejected() {
        let toml = r#"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
            mock_proving_time = { kind = "random", min_ms = 1000, max_ms = 50 }
        "#;
        let config = parse(toml);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_zero_proof_timeout_secs_rejected() {
        let toml = r#"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
            proof_timeout_secs = 0
        "#;
        let config = parse(toml);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_duplicate_proof_type_rejected() {
        let toml = r#"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
        "#;
        let config = parse(toml);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_parse_cluster() {
        let toml = r#"
            [[zkvm]]
            kind = "cluster"
            proof_type = "reth-zisk"
            endpoint = "http://zisk-cluster:50051"
        "#;
        let config = parse(toml);
        assert_eq!(config.zkvm.len(), 1);
        assert!(matches!(
            &config.zkvm[0],
            zkVMConfig::Cluster {
                endpoint,
                elf_path: None,
                elf_url: None,
                proof_timeout_secs: 12,
                ..
            } if endpoint == "http://zisk-cluster:50051"
        ));
        // The ELF is mandatory for the cluster backend, so a config that omits
        // both elf_path and elf_url parses but fails validation.
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cluster_non_zisk_rejected() {
        let toml = r#"
            [[zkvm]]
            kind = "cluster"
            proof_type = "reth-sp1"
            endpoint = "http://zisk-cluster:50051"
        "#;
        let config = parse(toml);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cluster_zero_proof_timeout_rejected() {
        let toml = r#"
            [[zkvm]]
            kind = "cluster"
            proof_type = "reth-zisk"
            endpoint = "http://zisk-cluster:50051"
            proof_timeout_secs = 0
        "#;
        let config = parse(toml);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cluster_with_elf_path() {
        let toml = r#"
            [[zkvm]]
            kind = "cluster"
            proof_type = "ethrex-zisk"
            endpoint = "http://zisk-cluster:50051"
            elf_path = "/tmp/stateless-validator-ethrex-zisk.elf"
        "#;
        let config = parse(toml);
        assert!(matches!(
            &config.zkvm[0],
            zkVMConfig::Cluster {
                elf_path: Some(path),
                elf_url: None,
                ..
            } if path.to_str() == Some("/tmp/stateless-validator-ethrex-zisk.elf")
        ));
        config.validate().unwrap();
    }

    #[test]
    fn test_cluster_with_elf_url() {
        let toml = r#"
            [[zkvm]]
            kind = "cluster"
            proof_type = "ethrex-zisk"
            endpoint = "http://zisk-cluster:50051"
            elf_url = "https://example.com/stateless-validator-ethrex-zisk.elf"
        "#;
        let config = parse(toml);
        assert!(matches!(
            &config.zkvm[0],
            zkVMConfig::Cluster {
                elf_path: None,
                elf_url: Some(url),
                ..
            } if url == "https://example.com/stateless-validator-ethrex-zisk.elf"
        ));
        config.validate().unwrap();
    }

    #[test]
    fn test_cluster_path_and_url_rejected() {
        let toml = r#"
            [[zkvm]]
            kind = "cluster"
            proof_type = "reth-zisk"
            endpoint = "http://zisk-cluster:50051"
            elf_path = "/tmp/x.elf"
            elf_url = "https://example.com/x.elf"
        "#;
        let config = parse(toml);
        assert!(config.validate().is_err());
    }
}

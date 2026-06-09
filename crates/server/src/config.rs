//! Configuration types.

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
const DEFAULT_WITNESS_TIMEOUT_SECS: u64 = 12;
const DEFAULT_PROOF_TIMEOUT_SECS: u64 = 12;
const DEFAULT_PROOF_CACHE_SIZE: usize = 128;
const DEFAULT_WITNESS_CACHE_SIZE: usize = 128;
const DEFAULT_MOCK_PROOF_SIZE: u64 = 128 << 10;
const DEFAULT_DASHBOARD_ENABLED: bool = false;
const DEFAULT_DASHBOARD_RETENTION: usize = 256;

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

fn default_mock_proving_time() -> MockProvingTime {
    MockProvingTime::Constant { ms: 6000 }
}

fn default_mock_proof_size() -> u64 {
    DEFAULT_MOCK_PROOF_SIZE
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
    /// Number of blocks to keep in the completed proofs LRU cache.
    #[serde(default = "default_proof_cache_size")]
    pub proof_cache_size: usize,
    /// Number of blocks to keep in the execution witness LRU cache.
    #[serde(default = "default_witness_cache_size")]
    pub witness_cache_size: usize,
    /// Dashboard feature configuration.
    #[serde(default)]
    pub dashboard: DashboardConfig,
    /// zkVM backend configurations.
    pub zkvm: Vec<zkVMConfig>,
}

impl Config {
    /// Load configuration from a TOML file at the given path.
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
        ensure!(self.proof_cache_size > 0, "proof_cache_size must be > 0");
        ensure!(
            self.witness_cache_size > 0,
            "witness_cache_size must be > 0"
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
                zkVMConfig::Verifier {
                    program_vk_path,
                    program_vk_url,
                    ..
                } => {
                    ensure!(
                        program_vk_path.is_some() || program_vk_url.is_some(),
                        "verifier zkvm {proof_type}: one of program_vk_path or program_vk_url must be set"
                    );
                    ensure!(
                        !(program_vk_path.is_some() && program_vk_url.is_some()),
                        "verifier zkvm {proof_type}: program_vk_path and program_vk_url are mutually exclusive"
                    );
                }
            }
            if let zkVMConfig::Mock {
                mock_proving_time,
                mock_proof_size,
                ..
            } = zkvm
            {
                ensure!(*mock_proof_size >= 32, "mock_proof_size must be >= 32");
                if let MockProvingTime::Random { min_ms, max_ms, .. } = mock_proving_time {
                    ensure!(
                        min_ms <= max_ms,
                        "mock_proving_time random: min_ms ({min_ms}) must be <= max_ms ({max_ms})"
                    );
                }
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

/// zkVM backend configuration. One of a remote ere-server, a mock, an
/// in-process verifier-only backend (no proving), or an external proving
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
        endpoint: String,
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
        /// Size of the mock proof in bytes.
        #[serde(default = "default_mock_proof_size")]
        mock_proof_size: u64,
        /// Whether the mock should always fail proof generation.
        #[serde(default)]
        mock_failure: bool,
    },
    /// In-process verifier-only backend. Verifies proofs received via HTTP
    /// without running an `ere-server` or pre-loading prover circuits.
    /// Returns an error on prove requests.
    Verifier {
        /// Proof type.
        proof_type: ProofType,
        /// Optional local file path to the program verifying key file (.vk) for
        /// the guest program of this proof type. Mutually exclusive with
        /// `program_vk_url`.
        #[serde(default)]
        program_vk_path: Option<PathBuf>,
        /// Optional URL to fetch the program verifying key file (.vk) from.
        /// Pre-computed and shipped in `eth-act/ere-guests` releases alongside
        /// the .elf. Mutually exclusive with `program_vk_path`.
        #[serde(default)]
        program_vk_url: Option<String>,
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
            | Self::Verifier { proof_type, .. }
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
        (None, None) => anyhow::bail!("either a local path or a URL must be set"),
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

    #[test]
    fn test_parse_multiple_zkvms() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"

            [[zkvm]]
            kind = "ere"
            endpoint = "http://ere-server:3000"
            proof_type = "ethrex-zisk"

            [[zkvm]]
            kind = "mock"
            proof_type = "reth-zisk"
            mock_proving_time = { kind = "constant", ms = 100 }
            mock_proof_size = 512
        "#;

        let config: Config = toml_edit::de::from_str(toml).unwrap();

        assert_eq!(config.zkvm.len(), 2);
        assert_eq!(config.zkvm[0].proof_type(), ProofType::EthrexZisk);
        assert_eq!(config.zkvm[1].proof_type(), ProofType::RethZisk);

        assert!(matches!(&config.zkvm[0], zkVMConfig::Ere { .. }));
        assert!(matches!(&config.zkvm[1], zkVMConfig::Mock { .. }));
    }

    #[test]
    fn test_defaults() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert_eq!(config.proof_cache_size, 128);
        assert_eq!(config.witness_cache_size, 128);
        assert!(matches!(
            config.zkvm[0],
            zkVMConfig::Mock {
                proof_timeout_secs: 12,
                mock_proving_time: MockProvingTime::Constant { ms: 6000 },
                mock_proof_size: 131072,
                ..
            }
        ));
    }

    #[test]
    fn test_empty_zkvm_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            zkvm = []
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_zero_proof_cache_size_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            proof_cache_size = 0
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_zero_witness_cache_size_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            witness_cache_size = 0
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_zero_dashboard_retention_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [dashboard]
            enabled = true
            retention = 0
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_random_proving_time_min_gt_max_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
            mock_proving_time = { kind = "random", min_ms = 1000, max_ms = 50 }
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_zero_proof_timeout_secs_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
            proof_timeout_secs = 0
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_duplicate_proof_type_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
            [[zkvm]]
            kind = "mock"
            proof_type = "reth-sp1"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_parse_cluster() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "cluster"
            proof_type = "reth-zisk"
            endpoint = "http://zisk-cluster:50051"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
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
    fn test_cluster_no_elf_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "cluster"
            proof_type = "reth-zisk"
            endpoint = "http://zisk-cluster:50051"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cluster_non_zisk_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "cluster"
            proof_type = "reth-sp1"
            endpoint = "http://zisk-cluster:50051"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cluster_zero_proof_timeout_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "cluster"
            proof_type = "reth-zisk"
            endpoint = "http://zisk-cluster:50051"
            proof_timeout_secs = 0
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cluster_with_elf_path() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "cluster"
            proof_type = "ethrex-zisk"
            endpoint = "http://zisk-cluster:50051"
            elf_path = "/tmp/stateless-validator-ethrex-zisk.elf"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(matches!(
            &config.zkvm[0],
            zkVMConfig::Cluster {
                elf_path: Some(p),
                elf_url: None,
                ..
            } if p.to_str() == Some("/tmp/stateless-validator-ethrex-zisk.elf")
        ));
        config.validate().unwrap();
    }

    #[test]
    fn test_cluster_with_elf_url() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "cluster"
            proof_type = "ethrex-zisk"
            endpoint = "http://zisk-cluster:50051"
            elf_url = "https://example.com/stateless-validator-ethrex-zisk.elf"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(matches!(
            &config.zkvm[0],
            zkVMConfig::Cluster {
                elf_path: None,
                elf_url: Some(u),
                ..
            } if u == "https://example.com/stateless-validator-ethrex-zisk.elf"
        ));
        config.validate().unwrap();
    }

    #[test]
    fn test_cluster_path_and_url_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "cluster"
            proof_type = "reth-zisk"
            endpoint = "http://zisk-cluster:50051"
            elf_path = "/tmp/x.vk"
            elf_url = "https://example.com/x.vk"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_verifier_with_program_vk_path() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "verifier"
            proof_type = "reth-zisk"
            program_vk_path = "/tmp/stateless-validator-reth-zisk.vk"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(matches!(
            &config.zkvm[0],
            zkVMConfig::Verifier {
                program_vk_path: Some(p),
                program_vk_url: None,
                ..
            } if p.to_str() == Some("/tmp/stateless-validator-reth-zisk.vk")
        ));
        config.validate().unwrap();
    }

    #[test]
    fn test_verifier_with_program_vk_url() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "verifier"
            proof_type = "reth-zisk"
            program_vk_url = "https://example.com/stateless-validator-reth-zisk.vk"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(matches!(
            &config.zkvm[0],
            zkVMConfig::Verifier {
                program_vk_path: None,
                program_vk_url: Some(u),
                ..
            } if u == "https://example.com/stateless-validator-reth-zisk.vk"
        ));
        config.validate().unwrap();
    }

    #[test]
    fn test_verifier_no_program_vk_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "verifier"
            proof_type = "reth-zisk"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_verifier_path_and_url_rejected() {
        let toml = r#"
            el_endpoint = "http://localhost:8545"
            [[zkvm]]
            kind = "verifier"
            proof_type = "reth-zisk"
            program_vk_path = "/tmp/x.vk"
            program_vk_url = "https://example.com/x.vk"
        "#;
        let config: Config = toml_edit::de::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }
}

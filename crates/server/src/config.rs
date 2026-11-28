use std::{
    collections::HashSet,
    fs,
    num::{NonZeroU64, NonZeroUsize},
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, ensure};
use ere_catalog::zkVMKind;
use serde::Deserialize;
use url::Url;

use crate::{beacon::execution_proof::MAX_PROOF_SIZE, proof::proof_type::ProofType};

/// Leaves room for the two SSZ offsets and the public values of a mock proof.
const MIN_MOCK_PROOF_SIZE: usize = 128;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    pub el_engine_endpoint: Url,
    /// Beacon API endpoint that receives the proofs.
    pub cl_beacon_endpoint: Url,
    /// EIP-2335 keystore of the validator that signs the proofs.
    pub validator_keystore_path: PathBuf,
    /// Plain text password of the keystore.
    pub validator_keystore_password_path: PathBuf,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    /// Without a zkVM, every request is forwarded unchanged.
    #[serde(default)]
    pub zkvm: Vec<ZkvmConfig>,
}

/// The live dashboard at `/dashboard`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DashboardConfig {
    pub enabled: bool,
    /// Number of recent blocks that the dashboard keeps.
    pub retention: NonZeroUsize,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            retention: NonZeroUsize::new(256).unwrap(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZkvmConfig {
    pub proof_type: ProofType,
    #[serde(default = "default_proof_timeout_secs")]
    pub proof_timeout_secs: NonZeroU64,
    #[serde(flatten)]
    pub backend: Backend,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Backend {
    /// A remote ere-server.
    Ere { endpoint: Url },
    /// An in-process mock that sleeps and returns a `MockProof`.
    Mock {
        #[serde(default = "default_mock_proving_time")]
        mock_proving_time: MockProvingTime,
        /// Size of the proof data in bytes.
        #[serde(default = "default_mock_proof_size")]
        mock_proof_size: usize,
        #[serde(default)]
        mock_failure: bool,
    },
    /// A remote ZisK or OpenVM proving cluster.
    Cluster {
        endpoint: String,
        /// Path of the guest ELF, exclusive with `elf_url`.
        elf_path: Option<PathBuf>,
        /// URL of the guest ELF, exclusive with `elf_path`.
        elf_url: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MockProvingTime {
    Constant {
        ms: u64,
    },
    /// Uniform in `[min_ms, max_ms]`.
    Random {
        min_ms: u64,
        max_ms: u64,
    },
    /// Proportional to the gas used by the block.
    Linear {
        ms_per_mgas: u64,
    },
}

fn default_port() -> u16 {
    3000
}

fn default_proof_timeout_secs() -> NonZeroU64 {
    NonZeroU64::new(12).unwrap()
}

fn default_mock_proving_time() -> MockProvingTime {
    MockProvingTime::Constant { ms: 6000 }
}

fn default_mock_proof_size() -> usize {
    256 << 10
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?
            .parse()
    }
}

impl FromStr for Config {
    type Err = anyhow::Error;

    fn from_str(toml: &str) -> anyhow::Result<Self> {
        let config: Self = toml_edit::de::from_str(toml)?;
        let mut proof_types = HashSet::new();
        for ZkvmConfig {
            proof_type,
            backend,
            ..
        } in &config.zkvm
        {
            ensure!(
                proof_types.insert(proof_type),
                "duplicate proof_type {proof_type}"
            );
            match backend {
                Backend::Ere { .. } => {}
                Backend::Mock {
                    mock_proving_time,
                    mock_proof_size,
                    ..
                } => {
                    ensure!(
                        (MIN_MOCK_PROOF_SIZE..=MAX_PROOF_SIZE).contains(mock_proof_size),
                        "{proof_type}: mock_proof_size must be in [{MIN_MOCK_PROOF_SIZE}, {MAX_PROOF_SIZE}]"
                    );
                    if let MockProvingTime::Random { min_ms, max_ms } = mock_proving_time {
                        ensure!(min_ms <= max_ms, "{proof_type}: min_ms must be <= max_ms");
                    }
                }
                Backend::Cluster {
                    elf_path, elf_url, ..
                } => {
                    ensure!(
                        matches!(proof_type.zkvm_kind(), zkVMKind::Zisk | zkVMKind::OpenVM),
                        "{proof_type}: a cluster proves zisk and openvm proof types only"
                    );
                    ensure!(
                        elf_path.is_some() != elf_url.is_some(),
                        "{proof_type}: set exactly one of elf_path and elf_url"
                    );
                }
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::config::{Backend, Config, MockProvingTime};

    const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

    fn parse(zkvm: &str) -> anyhow::Result<Config> {
        format!(
            r#"
            el_engine_endpoint = "http://localhost:8551"
            cl_beacon_endpoint = "http://localhost:4000"
            validator_keystore_path = "voting-keystore.json"
            validator_keystore_password_path = "password"
            {zkvm}
            "#
        )
        .parse()
    }

    /// The configuration of the README and of every example loads.
    #[test]
    fn test_documented_configs_load() {
        let readme = fs::read_to_string(format!("{ROOT}/README.md")).unwrap();
        let (_, toml) = readme.split_once("```toml\n").unwrap();
        let (toml, _) = toml.split_once("```").unwrap();
        let config: Config = toml.parse().unwrap();
        assert_eq!(config.zkvm.len(), 6);

        for example in ["mock-beacon-node", "observability", "testnet"] {
            let path = format!("{ROOT}/docker/example/{example}/zkboost/config.toml");
            Config::load(path.as_ref()).unwrap();
        }
    }

    #[test]
    fn test_defaults() {
        let config = parse(r#"zkvm = [{ kind = "mock", proof_type = "reth-sp1" }]"#).unwrap();
        assert_eq!(config.port, 3000);
        assert_eq!(config.zkvm[0].proof_timeout_secs.get(), 12);
        assert!(matches!(
            config.zkvm[0].backend,
            Backend::Mock {
                mock_proving_time: MockProvingTime::Constant { ms: 6000 },
                mock_proof_size: 262144,
                mock_failure: false,
            }
        ));
        let config = parse("[dashboard]\nenabled = true").unwrap();
        assert!(config.zkvm.is_empty() && config.dashboard.enabled);
        assert_eq!(config.dashboard.retention.get(), 256);
        assert!(parse("[dashboard]\nretention = 0").is_err());
    }

    /// Each zkVM entry loads or is rejected.
    #[test]
    fn test_zkvm_validation() {
        // An entry after `!` is rejected.
        let zkvms = r#"
              kind = "ere", proof_type = "reth-pico", endpoint = "http://ere:3000"
            ! kind = "ere", proof_type = "reth-pico"
            ! kind = "ere", proof_type = "reth-risc0", endpoint = "http://ere:3000"
            ! kind = "mock", proof_type = "reth-sp1", proof_timeout_secs = 0
            ! kind = "mock", proof_type = "reth-sp1", mock_proof_size = 127
              kind = "mock", proof_type = "reth-sp1", mock_proof_size = 128
              kind = "mock", proof_type = "reth-sp1", mock_proof_size = 4194304
            ! kind = "mock", proof_type = "reth-sp1", mock_proof_size = 4194305
            ! kind = "mock", proof_type = "reth-sp1", mock_proving_time = { kind = "random", min_ms = 9, max_ms = 1 }
              kind = "mock", proof_type = "reth-sp1", mock_proving_time = { kind = "linear", ms_per_mgas = 300 }
            ! kind = "cluster", endpoint = "http://c:7000", proof_type = "reth-zisk"
              kind = "cluster", endpoint = "http://c:7000", proof_type = "reth-zisk", elf_path = "a.elf"
              kind = "cluster", endpoint = "http://c:7000", proof_type = "reth-openvm", elf_url = "http://a/a.elf"
            ! kind = "cluster", endpoint = "http://c:7000", proof_type = "reth-sp1", elf_path = "a.elf"
            ! kind = "cluster", endpoint = "http://c:7000", proof_type = "reth-zisk", elf_path = "a.elf", elf_url = "http://a/a.elf"
        "#;
        for zkvm in zkvms.lines().map(str::trim).filter(|zkvm| !zkvm.is_empty()) {
            let (valid, zkvm) = zkvm
                .strip_prefix('!')
                .map_or((true, zkvm), |zkvm| (false, zkvm));
            let loads = parse(&format!("zkvm = [{{ {zkvm} }}]")).is_ok();
            assert_eq!(loads, valid, "{zkvm}");
        }
        let duplicate = r#"zkvm = [{ kind = "mock", proof_type = "reth-sp1" }, { kind = "mock", proof_type = "reth-sp1" }]"#;
        assert!(parse(duplicate).is_err());
    }
}

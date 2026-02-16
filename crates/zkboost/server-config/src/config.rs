//! Server configuration and TOML/YAML parsing.
//!
//! Defines the configuration structure for loading zkVM programs from TOML/YAML files.

use std::path::Path;

use anyhow::Context;
use ere_dockerized::zkVMKind;
use ere_zkvm_interface::ProverResource;
use serde::{Deserialize, Serialize};
use zkboost_types::ProgramID;

pub use crate::config::program::{PathConfig, ProgramConfig, UrlConfig};

mod program;

/// Server configuration loaded from a TOML/YAML file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Port to listen on.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Url for sending proof when generated.
    #[serde(default = "default_webhook_url")]
    pub webhook_url: String,
    /// List of zkVM programs to load on server startup.
    #[serde(default)]
    pub zkvm: Vec<zkVMConfig>,
}

fn default_port() -> u16 {
    3001
}

fn default_webhook_url() -> String {
    "http://localhost:3003/proofs".to_string()
}

impl Config {
    /// Load config from file (auto-detects format from extension).
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let string = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config at {path:?}"))?;

        match path.extension().and_then(|s| s.to_str()) {
            Some("toml") => Self::from_toml_str(&string),
            Some("yaml") => Self::from_yaml_str(&string),
            Some(ext) => anyhow::bail!("Unsupported config format: .{ext}"),
            None => anyhow::bail!("Config file must have an extension (e.g., .toml)"),
        }
    }

    /// Parse config from TOML string.
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        toml_edit::de::from_str(s)
            .with_context(|| format!("Failed to deserialize TOML config:\n{s}"))
    }

    /// Converts to TOML string.
    pub fn to_toml(&self) -> anyhow::Result<String> {
        let mut config_toml = toml_edit::ser::to_document(&self)?;

        // Format array into array of tables.
        if let Some(item) = config_toml.get_mut("zkvm")
            && let toml_edit::Item::Value(toml_edit::Value::Array(array)) = item
        {
            *item = array
                .iter()
                .map(|v| toml_edit::Table::from_iter(v.as_inline_table().unwrap()))
                .collect::<toml_edit::ArrayOfTables>()
                .into();
        }

        Ok(config_toml.to_string())
    }

    /// Parse config from YAML string.
    pub fn from_yaml_str(s: &str) -> anyhow::Result<Self> {
        serde_yaml::from_str(s).with_context(|| format!("Failed to deserialize YAML config:\n{s}"))
    }
}

/// Configuration for a single zkVM program.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(non_camel_case_types)]
pub enum zkVMConfig {
    /// Configuration for `DockerizedzkVM` ran by zkboost.
    Docker {
        /// The kind of zkVM backend to use.
        kind: zkVMKind,
        /// The compute resource type for proving (CPU, GPU, or network).
        resource: ProverResource,
        /// Unique identifier for this program.
        program_id: ProgramID,
        /// Path to the compiled program binary.
        program: ProgramConfig,
    },
    /// External Ere zkVM server configuration
    External {
        /// The endpoint URL of the remote Ere zkVM server.
        endpoint: String,
        /// Unique identifier for this program.
        program_id: ProgramID,
    },
    /// Mock zkVM configuration
    Mock {
        /// Mock time in millisecond for proof generation.
        mock_proving_time_ms: u64,
        /// Mock size for proof.
        mock_proof_size: u64,
        /// Unique identifier for this program.
        program_id: ProgramID,
    },
}

impl zkVMConfig {
    /// Returns [`ProgramID`].
    pub fn program_id(&self) -> &ProgramID {
        match self {
            Self::Docker { program_id, .. } => program_id,
            Self::External { program_id, .. } => program_id,
            Self::Mock { program_id, .. } => program_id,
        }
    }
}

#[cfg(test)]
mod test {
    use ere_dockerized::zkVMKind;
    use ere_zkvm_interface::{ProverResource, RemoteProverConfig};

    use crate::{Config, PathConfig, ProgramConfig, UrlConfig, zkVMConfig};

    #[test]
    fn test_from_toml_str() {
        let toml = r#"
            port = 3001
            webhook_url = "http://localhost:3003/proofs"

            [[zkvm]]
            kind = "openvm"
            resource = { kind = "cpu" }
            program_id = "openvm-test"
            program = "openvm-test-elf"

            [[zkvm]]
            kind = "sp1"
            resource = { kind = "network", endpoint = "http://localhost:3000", api_key = "secret" }
            program_id = "sp1-test"
            program = { path = "sp1-test-elf" }

            [[zkvm]]
            kind = "zisk"
            resource = { kind = "gpu" }
            program_id = "zisk-test"
            program = { url = "http://artifact" }

            [[zkvm]]
            endpoint = "http://remote:3000"
            program_id = "external-test"

            [[zkvm]]
            mock_proving_time_ms = 1000
            mock_proof_size = 1024
            program_id = "mock"
        "#;
        assert_eq!(Config::from_toml_str(toml).unwrap(), sample_config());
    }

    #[test]
    fn test_from_yaml_str() {
        let yaml = r#"
            port: 3001
            webhook_url: "http://localhost:3003/proofs"
            zkvm:
            - kind: openvm
              resource:
                kind: cpu
              program_id: openvm-test
              program: openvm-test-elf
            - kind: sp1
              resource:
                kind: network
                endpoint: http://localhost:3000
                api_key: secret
              program_id: sp1-test
              program:
                path: sp1-test-elf
            - kind: zisk
              resource:
                kind: gpu
              program_id: zisk-test
              program:
                url: http://artifact
            - endpoint: "http://remote:3000"
              program_id: "external-test"
            - mock_proving_time_ms: 1000
              mock_proof_size: 1024
              program_id: "mock"
        "#;
        assert_eq!(Config::from_yaml_str(yaml).unwrap(), sample_config());
    }

    fn sample_config() -> Config {
        Config {
            port: 3001,
            webhook_url: "http://localhost:3003/proofs".to_string(),
            zkvm: vec![
                zkVMConfig::Docker {
                    kind: zkVMKind::OpenVM,
                    resource: ProverResource::Cpu,
                    program_id: "openvm-test".into(),
                    program: ProgramConfig::Path("openvm-test-elf".into()),
                },
                zkVMConfig::Docker {
                    kind: zkVMKind::SP1,
                    resource: ProverResource::Network(RemoteProverConfig {
                        endpoint: "http://localhost:3000".to_string(),
                        api_key: Some("secret".to_string()),
                    }),
                    program_id: "sp1-test".into(),
                    program: ProgramConfig::ExplicitPath(PathConfig {
                        path: "sp1-test-elf".into(),
                    }),
                },
                zkVMConfig::Docker {
                    kind: zkVMKind::Zisk,
                    resource: ProverResource::Gpu,
                    program_id: "zisk-test".into(),
                    program: ProgramConfig::Url(UrlConfig {
                        url: "http://artifact".to_string(),
                    }),
                },
                zkVMConfig::External {
                    endpoint: "http://remote:3000".to_string(),
                    program_id: "external-test".into(),
                },
                zkVMConfig::Mock {
                    mock_proving_time_ms: 1000,
                    mock_proof_size: 1 << 10,
                    program_id: "mock".into(),
                },
            ],
        }
    }
}

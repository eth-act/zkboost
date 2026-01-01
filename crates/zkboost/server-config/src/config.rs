//! Server configuration and TOML/YAML parsing.
//!
//! Defines the configuration structure for loading zkVM programs from TOML/YAML files.

use std::path::Path;

use anyhow::Context;
use ere_dockerized::zkVMKind;
use ere_zkvm_interface::ProverResourceType;
use serde::{Deserialize, Serialize};
use zkboost_types::ProgramID;

pub use crate::config::program::{PathConfig, ProgramConfig, UrlConfig};

mod program;

/// Server configuration loaded from a TOML/YAML file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// List of zkVM programs to load on server startup.
    #[serde(default)]
    pub zkvm: Vec<zkVMConfig>,
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
        toml::from_str(s).with_context(|| format!("Failed to deserialize TOML config:\n{s}"))
    }

    /// Parse config from YAML string.
    pub fn from_yaml_str(s: &str) -> anyhow::Result<Self> {
        serde_yaml::from_str(s).with_context(|| format!("Failed to deserialize YAML config:\n{s}"))
    }
}

/// Configuration for a single zkVM program.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(non_camel_case_types)]
pub struct zkVMConfig {
    /// The kind of zkVM backend to use.
    pub kind: zkVMKind,
    /// The compute resource type for proving (CPU, GPU, or network).
    pub resource: ProverResourceType,
    /// Unique identifier for this program.
    pub program_id: ProgramID,
    /// Path to the compiled program binary.
    pub program: ProgramConfig,
}

#[cfg(test)]
mod test {
    use ere_dockerized::zkVMKind;
    use ere_zkvm_interface::{NetworkProverConfig, ProverResourceType};

    use crate::{Config, PathConfig, ProgramConfig, UrlConfig, zkVMConfig};

    #[test]
    fn test_from_toml_str() {
        let toml = r#"
            [[zkvm]]
            kind = "openvm"
            resource = "cpu"
            program-id = "openvm-test"
            program = "openvm-test-elf"

            [[zkvm]]
            kind = "sp1"
            resource = { network = { endpoint = "http://localhost:3000", api-key = "secret" } } 
            program-id = "sp1-test"
            program = { path = "sp1-test-elf" }

            [[zkvm]]
            kind = "zisk"
            resource = "gpu"
            program-id = "zisk-test"
            program = { url = "http://artifact" }
        "#;
        assert_eq!(Config::from_toml_str(toml).unwrap(), sample_config());
    }

    #[test]
    fn test_from_yaml_str() {
        // FIXME: After Ere add `#[serde(untagged)]` for `ProverResourceType`,
        //        remove the `!network` tagging.
        let yaml = r#"
            zkvm:
            - kind: openvm
              resource: cpu
              program-id: openvm-test
              program: openvm-test-elf
            - kind: sp1
              resource: !network
                endpoint: http://localhost:3000
                api-key: secret
              program-id: sp1-test
              program:
                path: sp1-test-elf
            - kind: zisk
              resource: gpu
              program-id: zisk-test
              program:
                url: http://artifact
        "#;
        assert_eq!(Config::from_yaml_str(yaml).unwrap(), sample_config());
    }

    fn sample_config() -> Config {
        Config {
            zkvm: vec![
                zkVMConfig {
                    kind: zkVMKind::OpenVM,
                    resource: ProverResourceType::Cpu,
                    program_id: "openvm-test".into(),
                    program: ProgramConfig::Path("openvm-test-elf".into()),
                },
                zkVMConfig {
                    kind: zkVMKind::SP1,
                    resource: ProverResourceType::Network(NetworkProverConfig {
                        endpoint: "http://localhost:3000".to_string(),
                        api_key: Some("secret".to_string()),
                    }),
                    program_id: "sp1-test".into(),
                    program: ProgramConfig::ExplicitPath(PathConfig {
                        path: "sp1-test-elf".into(),
                    }),
                },
                zkVMConfig {
                    kind: zkVMKind::Zisk,
                    resource: ProverResourceType::Gpu,
                    program_id: "zisk-test".into(),
                    program: ProgramConfig::Url(UrlConfig {
                        url: "http://artifact".to_string(),
                    }),
                },
            ],
        }
    }
}

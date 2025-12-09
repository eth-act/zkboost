use std::path::{Path, PathBuf};

use anyhow::Context;
use ere_dockerized::zkVMKind;
use ere_zkvm_interface::ProverResourceType;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub zkvm: Vec<zkVM>,
}

impl Config {
    /// Load config from file (auto-detects format from extension)
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let string = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config at {path:?}"))?;

        match path.extension().and_then(|s| s.to_str()) {
            Some("toml") => Self::from_toml_str(&string),
            Some(ext) => anyhow::bail!("Unsupported config format: .{ext}"),
            None => anyhow::bail!("Config file must have an extension (e.g., .toml)"),
        }
    }

    /// Parse config from TOML string
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        toml::from_str(s).with_context(|| format!("Failed to deserialize TOML config:\n{s}"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(non_camel_case_types)]
pub struct zkVM {
    pub kind: zkVMKind,
    pub resource: ProverResourceType,
    pub program_id: String,
    pub program_path: PathBuf,
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use ere_dockerized::zkVMKind;
    use ere_zkvm_interface::{NetworkProverConfig, ProverResourceType};

    use crate::config::{self, Config};

    #[test]
    fn test_from_toml_str() {
        let toml = r#"
            [[zkvm]]
            kind = "openvm"
            resource = "cpu"
            program-id = "openvm-test"
            program-path = "openvm-test-elf"

            [[zkvm]]
            kind = "sp1"
            resource = { network = { endpoint = "http://localhost:3000", api-key = "secret" } } 
            program-id = "sp1-test"
            program-path = "sp1-test-elf"

            [[zkvm]]
            kind = "zisk"
            resource = "gpu"
            program-id = "zisk-test"
            program-path = "zisk-test-elf"
        "#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(
            config.zkvm,
            [
                config::zkVM {
                    kind: zkVMKind::OpenVM,
                    resource: ProverResourceType::Cpu,
                    program_id: "openvm-test".to_string(),
                    program_path: PathBuf::from("openvm-test-elf"),
                },
                config::zkVM {
                    kind: zkVMKind::SP1,
                    resource: ProverResourceType::Network(NetworkProverConfig {
                        endpoint: "http://localhost:3000".to_string(),
                        api_key: Some("secret".to_string())
                    }),
                    program_id: "sp1-test".to_string(),
                    program_path: PathBuf::from("sp1-test-elf"),
                },
                config::zkVM {
                    kind: zkVMKind::Zisk,
                    resource: ProverResourceType::Gpu,
                    program_id: "zisk-test".to_string(),
                    program_path: PathBuf::from("zisk-test-elf"),
                }
            ]
        );
    }
}

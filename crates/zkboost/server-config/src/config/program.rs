use std::{fs, path::PathBuf};

use anyhow::{Context, bail};
use ere_dockerized::SerializedProgram;
use serde::{Deserialize, Serialize};

/// Configuration for how to load a zkVM program.
///
/// Supports multiple formats:
/// - Simple string path e.g. `program = "./elf/reth-zisk"`
/// - Explicit path object e.g. `program = { path = "./elf/reth-zisk" }`
/// - URL download e.g. `program = { url = "https://github.com/eth-act/zkevm-benchmark-workload/releases/v0.1.0/reth-zisk" }`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProgramConfig {
    /// Simple string path to a local program file.
    Path(PathBuf),

    /// Explicit path object with named field.
    ExplicitPath(PathConfig),

    /// Remote URL to download the program from.
    Url(UrlConfig),
}

impl ProgramConfig {
    /// Load the program from either a local path or remote URL.
    pub async fn load(&self) -> anyhow::Result<SerializedProgram> {
        let bytes = match self {
            ProgramConfig::Path(path) | ProgramConfig::ExplicitPath(PathConfig { path }) => {
                fs::read(path).with_context(|| {
                    format!("Failed to read program from path: {}", path.display())
                })?
            }
            ProgramConfig::Url(UrlConfig { url }) => {
                let response = reqwest::get(url)
                    .await
                    .with_context(|| format!("Failed to download program from URL: {url}"))?;

                let status = response.status();
                if !status.is_success() {
                    bail!("Failed to download program from URL: {url} (HTTP status: {status})");
                }

                response
                    .bytes()
                    .await
                    .with_context(|| format!("Failed to read response bytes from URL: {url}"))?
                    .to_vec()
            }
        };
        Ok(SerializedProgram(bytes))
    }
}

/// Path configuration for explicit path object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathConfig {
    /// Path to the file
    pub path: PathBuf,
}

/// URL configuration for program download.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UrlConfig {
    /// Url to the file
    pub url: String,
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use crate::{PathConfig, ProgramConfig, UrlConfig};

    #[derive(Deserialize)]
    struct TestConfig {
        program: ProgramConfig,
    }

    #[test]
    fn test_parse_simple_path() {
        let toml = r#"program = "./elf/reth-zisk""#;
        let config: TestConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            config.program,
            ProgramConfig::Path("./elf/reth-zisk".into())
        );
    }

    #[test]
    fn test_parse_explicit_path() {
        let toml = r#"program = { path = "./elf/reth-zisk" }"#;
        let config: TestConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            config.program,
            ProgramConfig::ExplicitPath(PathConfig {
                path: "./elf/reth-zisk".into(),
            })
        );
    }

    #[test]
    fn test_parse_url() {
        let toml = r#"program = { url = "https://example.com/program" }"#;
        let config: TestConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            config.program,
            ProgramConfig::Url(UrlConfig {
                url: "https://example.com/program".to_string(),
            })
        );
    }

    #[test]
    fn test_reject_both_path_and_url() {
        let toml = r#"program = { path = "./elf/program", url = "https://example.com/program" }"#;
        assert!(toml::from_str::<TestConfig>(toml).is_err());
    }

    #[test]
    fn test_reject_neither_path_nor_url() {
        let toml = r#"program = {}"#;
        assert!(toml::from_str::<TestConfig>(toml).is_err());
    }
}

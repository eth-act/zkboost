use std::{fs, path::PathBuf};

use anyhow::{Context, bail};
use ere_dockerized::SerializedProgram;
use serde::{Deserialize, Serialize};

use crate::minisig::verify_minisig;

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
    pub async fn load(&self, verifier_key: Option<&str>) -> anyhow::Result<SerializedProgram> {
        let (program_bytes, signature) = match self {
            ProgramConfig::Path(path) | ProgramConfig::ExplicitPath(PathConfig { path }) => {
                self.load_from_path(path).await?
            }
            ProgramConfig::Url(UrlConfig { url }) => self.load_from_url(url).await?,
        };

        if let Some(key) = verifier_key {
            let signature =
                signature.ok_or_else(|| anyhow::anyhow!("Missing signature for verification"))?;
            verify_minisig(signature.trim(), key, &program_bytes)
                .with_context(|| "Failed to verify mini-signature")?;
        }

        Ok(SerializedProgram(program_bytes))
    }

    async fn load_from_path(
        &self,
        path: &std::path::Path,
    ) -> anyhow::Result<(Vec<u8>, Option<String>)> {
        let program_bytes = fs::read(path)
            .with_context(|| format!("Failed to read program from path: {}", path.display()))?;

        let sig_path = path.with_extension(".minisig");
        let signature = if sig_path.exists() {
            Some(fs::read_to_string(&sig_path).with_context(|| {
                format!(
                    "Failed to read mini-signature from path: {}",
                    sig_path.display()
                )
            })?)
        } else {
            None
        };

        Ok((program_bytes, signature))
    }

    async fn load_from_url(&self, url: &str) -> anyhow::Result<(Vec<u8>, Option<String>)> {
        let response = reqwest::get(url)
            .await
            .with_context(|| format!("Failed to download program from URL: {url}"))?;

        let status = response.status();
        if !status.is_success() {
            bail!("Failed to download program from URL: {url} (HTTP status: {status})");
        }

        let program_bytes = response
            .bytes()
            .await
            .with_context(|| format!("Failed to read response bytes from URL: {url}"))?
            .to_vec();

        let sig_url = format!("{}.minisig", url);
        let signature = download_text(&sig_url).await.ok();

        Ok((program_bytes, signature))
    }
}

async fn download_text(url: &str) -> anyhow::Result<String> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("Failed to download text from URL: {url}"))?;

    let status = response.status();
    if !status.is_success() {
        bail!("Failed to download text from URL: {url} (HTTP status: {status})");
    }

    response
        .text()
        .await
        .with_context(|| format!("Failed to read response text from URL: {url}"))
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
        let config: TestConfig = toml_edit::de::from_str(toml).unwrap();
        assert_eq!(
            config.program,
            ProgramConfig::Path("./elf/reth-zisk".into())
        );
    }

    #[test]
    fn test_parse_explicit_path() {
        let toml = r#"program = { path = "./elf/reth-zisk" }"#;
        let config: TestConfig = toml_edit::de::from_str(toml).unwrap();
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
        let config: TestConfig = toml_edit::de::from_str(toml).unwrap();
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
        assert!(toml_edit::de::from_str::<TestConfig>(toml).is_err());
    }

    #[test]
    fn test_reject_neither_path_nor_url() {
        let toml = r#"program = {}"#;
        assert!(toml_edit::de::from_str::<TestConfig>(toml).is_err());
    }

    #[tokio::test]
    async fn test_load_program() {
        let config = ProgramConfig::Url(UrlConfig {
            url: "https://github.com/eth-act/ere-guests/releases/download/v0.4.0/block-encoding-length-airbender".to_string(),
        });
        let verifier_key = Some("RWTsNA0kZFhw19A26aujYun4hv4RraCnEYDehrgEG6NnCjmjkr9/+KGy");
        let program = config.load(verifier_key).await;
        assert!(program.is_ok());
    }
}

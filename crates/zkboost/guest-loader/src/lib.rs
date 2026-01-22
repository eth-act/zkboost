//! Guest program loader, loading and verifying guest program ELF and signature
use anyhow::{Context, Result, anyhow};
use minisign_verify::{PublicKey, Signature};
use reqwest::Client;
use zkboost_server_config::{PathConfig, ProgramConfig, UrlConfig};

/// Responsible for fetching and verifying guest program artifacts.
#[derive(Debug, Clone, Default)]
pub struct GuestLoader {
    client: Client,
}

impl GuestLoader {
    /// Creates a new GuestLoader instance.
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    /// Fetches the program and its signature, verifies the signature using the public key,
    /// and returns the verified program bytes.
    pub async fn load_and_verify(
        &self,
        program_source: &ProgramConfig,
        signature_source: &ProgramConfig,
        publisher_public_key: &str,
    ) -> Result<Vec<u8>> {
        let public_key = PublicKey::from_base64(publisher_public_key)
            .map_err(|_| anyhow!("Invalid base64 public key"))?;
        let program_bytes = self.fetch_program_bytes(program_source).await?;
        let signature_str = self.fetch_program_string(signature_source).await?;
        let signature =
            Signature::decode(&signature_str).map_err(|_| anyhow!("Failed to decode signature"))?;
        public_key
            .verify(&program_bytes, &signature, false)
            .map_err(|_| anyhow!("Signature verification failed"))?;

        Ok(program_bytes)
    }

    async fn fetch_program_bytes(&self, source: &ProgramConfig) -> Result<Vec<u8>> {
        match source {
            ProgramConfig::Url(UrlConfig { url }) => {
                let response = self
                    .client
                    .get(url)
                    .send()
                    .await
                    .context("Failed to fetch artifact")?;
                let bytes = response
                    .error_for_status()
                    .context("HTTP error response")?
                    .bytes()
                    .await
                    .context("Failed to read response bytes")?;
                Ok(bytes.to_vec())
            }
            ProgramConfig::Path(path) | ProgramConfig::ExplicitPath(PathConfig { path }) => {
                tokio::fs::read(path).await.context("Failed to read file")
            }
        }
    }

    async fn fetch_program_string(&self, source: &ProgramConfig) -> Result<String> {
        match source {
            ProgramConfig::Url(UrlConfig { url }) => {
                let response = self
                    .client
                    .get(url)
                    .send()
                    .await
                    .context("Failed to fetch artifact")?;
                let text = response
                    .error_for_status()
                    .context("HTTP error response")?
                    .text()
                    .await
                    .context("Failed to read response text")?;
                Ok(text)
            }
            ProgramConfig::Path(path) | ProgramConfig::ExplicitPath(PathConfig { path }) => {
                tokio::fs::read_to_string(path)
                    .await
                    .context("Failed to read file")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_verify_minisig_downloaded_airbender() {
        let base_url = "https://github.com/eth-act/ere-guests/releases/download/v0.4.0";
        let pub_key_url = format!("{base_url}/minisign.pub");
        let sig_url = format!("{base_url}/block-encoding-length-airbender.minisig");
        let program_url = format!("{base_url}/block-encoding-length-airbender");

        let pub_key_str = reqwest::get(&pub_key_url)
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        let pub_key_lines: Vec<&str> = pub_key_str.lines().collect();
        let pub_key_base64 = pub_key_lines.last().unwrap().trim();

        let loader = GuestLoader::new();
        let program_config =
            ProgramConfig::Url(zkboost_server_config::UrlConfig { url: program_url });
        let sig_config = ProgramConfig::Url(zkboost_server_config::UrlConfig { url: sig_url });

        let result = loader
            .load_and_verify(&program_config, &sig_config, pub_key_base64)
            .await;
        assert!(
            result.is_ok(),
            "Verification failed for Airbender: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_verify_minisig_airbender_hardcoded_key() {
        let base_url = "https://github.com/eth-act/ere-guests/releases/download/v0.4.0";
        let sig_url = format!("{base_url}/block-encoding-length-airbender.minisig");
        let program_url = format!("{base_url}/block-encoding-length-airbender");

        let pub_key_str = "RWTsNA0kZFhw19A26aujYun4hv4RraCnEYDehrgEG6NnCjmjkr9/+KGy";

        let loader = GuestLoader::new();
        let program_config =
            ProgramConfig::Url(zkboost_server_config::UrlConfig { url: program_url });
        let sig_config = ProgramConfig::Url(zkboost_server_config::UrlConfig { url: sig_url });

        let result = loader
            .load_and_verify(&program_config, &sig_config, pub_key_str.trim())
            .await;
        assert!(
            result.is_ok(),
            "Verification failed for Airbender (hardcoded key): {:?}",
            result.err()
        );
    }
}

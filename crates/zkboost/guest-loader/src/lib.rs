//! Guest program loader, loading and verifying guest program ELF and signature
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use minisign::{PublicKey, SignatureBox};
use reqwest::Client;

/// Trait for HTTP client
pub trait HttpClient {
    /// Fetches bytes from the given URL.
    fn get_bytes(&self, url: &str) -> impl std::future::Future<Output = Result<Vec<u8>>> + Send;
    /// Fetches string from the given URL.
    fn get_string(&self, url: &str) -> impl std::future::Future<Output = Result<String>> + Send;
}

impl HttpClient for &Client {
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
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

    async fn get_string(&self, url: &str) -> Result<String> {
        let response = self
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
}

/// Fetches the program and its signature, verifies the signature using the public key,
/// and returns the verified program bytes.
pub async fn load_and_verify_with_url(
    program_url: &str,
    signature_url: &str,
    publisher_public_key: &str,
    client: &impl HttpClient,
) -> Result<Vec<u8>> {
    let program_bytes = fetch_bytes_with_url(program_url, client).await?;
    let signature_str = fetch_string_with_url(signature_url, client).await?;

    verify_program_and_signature(&program_bytes, &signature_str, publisher_public_key)?;

    Ok(program_bytes)
}

/// Verifies the signature using the public key.
/// This is empolyed when program and signature have been download already
pub fn verify_program_and_signature(
    program_bytes: &[u8],
    signature: &str,
    publisher_public_key: &str,
) -> Result<()> {
    let public_key = PublicKey::from_base64(publisher_public_key)
        .map_err(|_| anyhow!("Invalid base64 public key"))?;
    let signature_box =
        SignatureBox::from_string(signature).map_err(|_| anyhow!("Failed to decode signature"))?;

    minisign::verify(
        &public_key,
        &signature_box,
        std::io::Cursor::new(program_bytes),
        true,
        false,
        false,
    )
    .map_err(|_| anyhow!("Signature verification failed"))?;

    Ok(())
}

/// Fetches the program bytes from the given URL.
pub async fn fetch_bytes_with_url(url: &str, client: &impl HttpClient) -> Result<Vec<u8>> {
    let response = client.get_bytes(url).await?;
    Ok(response)
}

/// Fetches the string from the given URL.
pub async fn fetch_string_with_url(url: &str, client: &impl HttpClient) -> Result<String> {
    let response = client.get_string(url).await?;
    Ok(response)
}

/// Fetched bytes with Path
pub async fn fetch_bytes_with_path(path: &PathBuf) -> Result<Vec<u8>> {
    let bytes: Vec<u8> = tokio::fs::read(path)
        .await
        .context("Failed to read artifact")?;
    Ok(bytes)
}

/// Fetched string with Path
pub async fn fetch_string_with_path(path: &PathBuf) -> Result<String> {
    let text: String = tokio::fs::read_to_string(path)
        .await
        .context("Failed to read artifact")?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use minisign::KeyPair;

    use super::*;

    struct MockHttpClient {
        bytes_responses: std::collections::HashMap<String, Vec<u8>>,
        string_responses: std::collections::HashMap<String, String>,
    }

    impl MockHttpClient {
        fn new() -> Self {
            Self {
                bytes_responses: std::collections::HashMap::new(),
                string_responses: std::collections::HashMap::new(),
            }
        }
    }

    impl HttpClient for MockHttpClient {
        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
            self.bytes_responses
                .get(url)
                .cloned()
                .ok_or_else(|| anyhow!("Url not found: {url}"))
        }

        async fn get_string(&self, url: &str) -> Result<String> {
            self.string_responses
                .get(url)
                .cloned()
                .ok_or_else(|| anyhow!("Url not found: {url}"))
        }
    }

    #[test]
    fn test_verify_program_and_signature() {
        let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
        let pk_str = keypair.pk.to_base64();

        let program_data = b"test program data".to_vec();

        let reader = Cursor::new(program_data.clone());
        let signature_box = minisign::sign(None, &keypair.sk, reader, None, None).unwrap();
        let sig_str = signature_box.to_string();

        let result = verify_program_and_signature(&program_data, &sig_str, &pk_str);
        assert!(
            result.is_ok(),
            "Verification failed for generated keys: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_load_and_verify_with_url() {
        let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
        let pk_str = keypair.pk.to_base64();
        let program_data = b"test program data".to_vec();
        let reader = Cursor::new(program_data.clone());
        let signature_box = minisign::sign(None, &keypair.sk, reader, None, None).unwrap();
        let sig_str = signature_box.to_string();
        let mut client = MockHttpClient::new();
        let program_url = "http://example.com/program.elf";
        let signature_url = "http://example.com/program.sig";
        client
            .bytes_responses
            .insert(program_url.to_string(), program_data.clone());
        client
            .string_responses
            .insert(signature_url.to_string(), sig_str.clone());
        let result = load_and_verify_with_url(program_url, signature_url, &pk_str, &client).await;

        assert!(
            result.is_ok(),
            "Verification failed for generated keys: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap(), program_data);
    }
}

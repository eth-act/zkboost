//! Mini-signature verification implementation
use minisign::{PublicKey, SignatureBox};
use std::io::Cursor;

/// Verifies a minisign signature against a public key and program data.
pub fn verify_minisig(
    signature_str: &str,
    public_key_str: &str,
    program: &[u8],
) -> Result<(), anyhow::Error> {
    let signature = SignatureBox::from_string(signature_str)?;

    let public_key_str_clean = if public_key_str.contains("\n") {
        public_key_str
            .lines()
            .last()
            .unwrap_or(public_key_str)
            .trim()
    } else {
        public_key_str.trim()
    };

    let public_key = PublicKey::from_base64(public_key_str_clean)?;
    let data_reader = Cursor::new(program);
    minisign::verify(&public_key, &signature, data_reader, true, false, false)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use minisign::KeyPair;

    #[test]
    fn test_verify_minisig_generated_keys() {
        let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
        let pk_str = keypair.pk.to_base64();
        let program_data = b"test program data".to_vec();

        let reader = Cursor::new(program_data.clone());

        let signature_box = minisign::sign(None, &keypair.sk, reader, None, None).unwrap();
        let sig_str = signature_box.to_string();

        let result = verify_minisig(&sig_str, &pk_str, &program_data);
        assert!(
            result.is_ok(),
            "Verification failed for generated keys: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_verify_minisig_downloaded_airbender() {
        let base_url = "https://github.com/eth-act/ere-guests/releases/download/v0.4.0";
        let pub_key_url = format!("{}/minisign.pub", base_url);
        let sig_url = format!("{}/block-encoding-length-airbender.minisig", base_url);
        let program_url = format!("{}/block-encoding-length-airbender", base_url);

        // helper to download text
        async fn download_text(url: &str) -> String {
            reqwest::get(url).await.unwrap().text().await.unwrap()
        }

        // helper to download bytes
        async fn download_bytes(url: &str) -> Vec<u8> {
            reqwest::get(url)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .to_vec()
        }

        let pub_key_str = download_text(&pub_key_url).await;
        let sig_str = download_text(&sig_url).await;
        let program_bytes = download_bytes(&program_url).await;

        let result = verify_minisig(sig_str.trim(), pub_key_str.trim(), &program_bytes);
        assert!(
            result.is_ok(),
            "Verification failed for Airbender: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_verify_minisig_airbender_hardcoded_key() {
        let base_url = "https://github.com/eth-act/ere-guests/releases/download/v0.4.0";
        let sig_url = format!("{}/block-encoding-length-airbender.minisig", base_url);
        let program_url = format!("{}/block-encoding-length-airbender", base_url);

        let pub_key_str = "RWTsNA0kZFhw19A26aujYun4hv4RraCnEYDehrgEG6NnCjmjkr9/+KGy";

        // helper to download text
        async fn download_text(url: &str) -> String {
            reqwest::get(url).await.unwrap().text().await.unwrap()
        }

        // helper to download bytes
        async fn download_bytes(url: &str) -> Vec<u8> {
            reqwest::get(url)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .to_vec()
        }

        let sig_str = download_text(&sig_url).await;
        let program_bytes = download_bytes(&program_url).await;

        let result = verify_minisig(sig_str.trim(), pub_key_str.trim(), &program_bytes);
        assert!(
            result.is_ok(),
            "Verification failed for Airbender (hardcoded key): {:?}",
            result.err()
        );
    }
}

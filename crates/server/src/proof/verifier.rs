//! In-process verifier-only backends.
//!
//! Wraps the per-zkVM `ere-verifier-*` crates so zkboost can verify proofs
//! without a remote `ere-server` (which loads the full prover circuit). Each
//! verifier is bound to a specific compiled guest program via its
//! `program_vk`, downloaded from the URL configured for that proof_type.

use anyhow::Context;
#[cfg(feature = "verifier-zisk")]
use ere_verifier_core::{PublicValues, codec::Decode, zkVMVerifier};
use zkboost_types::ProofType;

/// Per-zkVM verifier dispatch. Variants are feature-gated.
#[allow(non_camel_case_types)]
pub(crate) enum DynVerifier {
    /// In-process ZisK verifier.
    #[cfg(feature = "verifier-zisk")]
    Zisk(ere_verifier_zisk::ZiskVerifier),
}

impl std::fmt::Debug for DynVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "verifier-zisk")]
            Self::Zisk(_) => f.write_str("DynVerifier::Zisk"),
            #[cfg(not(any(feature = "verifier-zisk")))]
            _ => f.write_str("DynVerifier::<empty>"),
        }
    }
}

impl DynVerifier {
    /// Construct a verifier for the given proof_type by downloading the
    /// program verifying key from `url` and decoding it.
    pub(crate) async fn from_url(proof_type: ProofType, url: &str) -> anyhow::Result<Self> {
        let bytes = download_program_vk(url).await?;
        Self::from_bytes(proof_type, &bytes)
    }

    fn from_bytes(proof_type: ProofType, bytes: &[u8]) -> anyhow::Result<Self> {
        match proof_type {
            #[cfg(feature = "verifier-zisk")]
            ProofType::EthrexZisk | ProofType::RethZisk => {
                let program_vk = ere_verifier_zisk::ZiskProgramVk::decode_from_slice(bytes)
                    .with_context(|| format!("decode ZiskProgramVk for {proof_type}"))?;
                Ok(Self::Zisk(ere_verifier_zisk::ZiskVerifier::new(program_vk)))
            }
            #[allow(unreachable_patterns)]
            _ => anyhow::bail!(
                "no in-process verifier compiled in for proof_type {proof_type}; \
                 enable the matching `verifier-*` feature on `zkboost-server`"
            ),
        }
    }

    /// Verify a serialized proof and return its public values.
    pub(crate) async fn verify(&self, proof: &[u8]) -> anyhow::Result<Vec<u8>> {
        match self {
            #[cfg(feature = "verifier-zisk")]
            Self::Zisk(verifier) => {
                let proof = ere_verifier_zisk::ZiskProof::decode_from_slice(proof)
                    .context("decode ZiskProof")?;
                let public_values: PublicValues = verifier
                    .verify(&proof)
                    .map_err(|error| anyhow::anyhow!("zisk verify failed: {error}"))?;
                Ok(Vec::<u8>::from(public_values))
            }
        }
    }
}

async fn download_program_vk(url: &str) -> anyhow::Result<Vec<u8>> {
    if let Some(path) = url
        .strip_prefix("file://")
        .or_else(|| if url.contains("://") { None } else { Some(url) })
    {
        return std::fs::read(path).with_context(|| format!("read program_vk from {path}"));
    }
    let bytes = reqwest::get(url)
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

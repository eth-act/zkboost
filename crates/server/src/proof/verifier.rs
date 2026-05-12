//! In-process verifier-only backends.
//!
//! Wraps the per-zkVM `ere-verifier-*` crates so zkboost can verify proofs
//! without a remote `ere-server` (which loads the full prover circuit). Each
//! verifier is bound to a specific compiled guest program via its
//! `program_vk`, downloaded from the URL configured for that proof_type.

use anyhow::Context;
use ere_verifier::Verifier;
use zkboost_types::ProofType;

pub(crate) async fn verifier_from_url(
    proof_type: ProofType,
    url: &str,
) -> anyhow::Result<Verifier> {
    let encoded_program_vk = download_program_vk(url).await?;
    Ok(Verifier::new(proof_type.zkvm_kind(), &encoded_program_vk)?)
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

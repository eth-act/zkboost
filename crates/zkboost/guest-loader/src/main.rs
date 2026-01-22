//! CLI tool for verifying guest program artifacts.
//!
//! This tool fetches a program ELF and its signature from URLs or local paths,
//! verifies the signature against a given public key, and saves the verified
//! program to an output file.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use guest_loader::verify_program_and_signature;
use reqwest::Client;
use tokio::fs;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// URL or path to the program ELF
    #[arg(long, short = 'p')]
    program: String,

    /// URL or path to the signature
    #[arg(long, short = 's')]
    signature: String,

    /// URL, path, or direct string for the public key
    #[arg(long, short = 'k')]
    public_key: String,

    /// Output path for the verified program
    #[arg(long, short = 'o')]
    output: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = Client::new();

    println!("Fetching program from: {}", args.program);
    let program_bytes = fetch_artifact_bytes(&args.program, &client).await?;

    println!("Fetching signature from: {}", args.signature);
    let signature_str = fetch_artifact_string(&args.signature, &client).await?;
    let signature_str = signature_str.trim().to_string();

    println!("Resolving public key...");
    let public_key_str = resolve_public_key(&args.public_key, &client).await?;
    let public_key_str = public_key_str.trim().to_string();

    println!("Verifying program and signature...");
    verify_program_and_signature(&program_bytes, &signature_str, &public_key_str)
        .context("Verification failed")?;

    println!("Verification successful!");

    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&args.output, &program_bytes).await?;
    println!("Verified program written to: {:?}", args.output);

    Ok(())
}

async fn fetch_artifact_bytes(source: &str, client: &Client) -> Result<Vec<u8>> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let response = client.get(source).send().await?.error_for_status()?;
        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    } else {
        let path = PathBuf::from(source);
        let bytes = fs::read(&path)
            .await
            .with_context(|| format!("Failed to read file: {:?}", path))?;
        Ok(bytes)
    }
}

async fn fetch_artifact_string(source: &str, client: &Client) -> Result<String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let response = client.get(source).send().await?.error_for_status()?;
        let text = response.text().await?;
        Ok(text)
    } else {
        let path = PathBuf::from(source);
        let text = fs::read_to_string(&path)
            .await
            .with_context(|| format!("Failed to read file: {:?}", path))?;
        Ok(text)
    }
}

async fn resolve_public_key(source: &str, client: &Client) -> Result<String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        return fetch_artifact_string(source, client).await;
    }

    let path = PathBuf::from(source);
    if path.exists() {
        return fetch_artifact_string(source, client).await;
    }

    // Assume it's the raw key string
    Ok(source.to_string())
}

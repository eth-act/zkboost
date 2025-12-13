use anyhow::{Context, Result};
use reqwest::Client as HttpClient;
use zkboost_types::{
    ExecuteRequest, ExecuteResponse, ProgramID, ProveRequest, ProveResponse, ServerInfoResponse,
    VerifyRequest, VerifyResponse,
};

/// HTTP client for zkboost servers.
///
/// Provides methods to execute programs, generate proofs, and verify proofs.
#[allow(non_camel_case_types)]
#[derive(Clone, Debug)]
pub struct zkBoostClient {
    base_url: String,
    http_client: HttpClient,
}

impl zkBoostClient {
    /// Creates a new client connected to the specified server URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http_client: HttpClient::new(),
        }
    }

    /// Creates a new client with a custom HTTP client.
    pub fn with_http_client(base_url: impl Into<String>, http_client: HttpClient) -> Self {
        Self {
            base_url: base_url.into(),
            http_client,
        }
    }

    /// Executes a program without generating a proof.
    pub async fn execute(
        &self,
        program_id: impl Into<ProgramID>,
        input: Vec<u8>,
    ) -> Result<ExecuteResponse> {
        let request = ExecuteRequest {
            program_id: program_id.into(),
            input,
        };

        let response = self
            .http_client
            .post(format!("{}/execute", self.base_url))
            .json(&request)
            .send()
            .await
            .context("Failed to send execute request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_msg = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Execute request failed with status {status}: {error_msg}");
        }

        response
            .json::<ExecuteResponse>()
            .await
            .context("Failed to parse execute response")
    }

    /// Generates a proof for a program execution.
    pub async fn prove(
        &self,
        program_id: impl Into<ProgramID>,
        input: Vec<u8>,
    ) -> Result<ProveResponse> {
        let request = ProveRequest {
            program_id: program_id.into(),
            input,
        };

        let response = self
            .http_client
            .post(format!("{}/prove", self.base_url))
            .json(&request)
            .send()
            .await
            .context("Failed to send prove request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_msg = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Prove request failed with status {status}: {error_msg}");
        }

        response
            .json::<ProveResponse>()
            .await
            .context("Failed to parse prove response")
    }

    /// Verifies a proof without re-executing the program.
    pub async fn verify(
        &self,
        program_id: impl Into<ProgramID>,
        proof: Vec<u8>,
    ) -> Result<VerifyResponse> {
        let request = VerifyRequest {
            program_id: program_id.into(),
            proof,
        };

        let response = self
            .http_client
            .post(format!("{}/verify", self.base_url))
            .json(&request)
            .send()
            .await
            .context("Failed to send verify request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_msg = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Verify request failed with status {status}: {error_msg}");
        }

        response
            .json::<VerifyResponse>()
            .await
            .context("Failed to parse verify response")
    }

    /// Retrieves server hardware and system information.
    pub async fn info(&self) -> Result<ServerInfoResponse> {
        let response = self
            .http_client
            .get(format!("{}/info", self.base_url))
            .send()
            .await
            .context("Failed to send info request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_msg = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!("Info request failed with status {status}: {error_msg}");
        }

        response
            .json::<ServerInfoResponse>()
            .await
            .context("Failed to parse info response")
    }
}

use reqwest::{Client, IntoUrl, RequestBuilder, Response, Url};
use serde::{Serialize, de::DeserializeOwned};
use zkboost_types::{
    ExecuteRequest, ExecuteResponse, ProgramID, ProveRequest, ProveResponse, ServerInfoResponse,
    VerifyRequest, VerifyResponse,
};

use crate::Error;

/// HTTP client for zkboost servers.
///
/// Provides methods to execute programs, generate proofs, and verify proofs.
#[allow(non_camel_case_types)]
#[derive(Clone, Debug)]
pub struct zkboostClient {
    base_url: Url,
    client: Client,
}

impl zkboostClient {
    /// Creates a new client connected to the specified server URL.
    pub fn new(base_url: impl IntoUrl) -> Result<Self, Error> {
        Ok(Self {
            base_url: base_url.into_url()?,
            client: Client::new(),
        })
    }

    /// Creates a new client with a custom [`reqwest::Client`].
    pub fn with_client(base_url: impl IntoUrl, client: Client) -> Result<Self, Error> {
        Ok(Self {
            base_url: base_url.into_url()?,
            client,
        })
    }

    /// Sends a GET request to the specified path and deserializes the response.
    pub async fn get<Res: DeserializeOwned>(&self, path: &'static str) -> Result<Res, Error> {
        let res = send(self.client.get(self.base_url.join(path).unwrap())).await?;
        Ok(res.json::<Res>().await?)
    }

    /// Sends a POST request with a JSON body and deserializes the response.
    pub async fn post<Req: Serialize, Res: DeserializeOwned>(
        &self,
        path: &'static str,
        req: Req,
    ) -> Result<Res, Error> {
        let res = send(
            self.client
                .post(self.base_url.join(path).unwrap())
                .json(&req),
        )
        .await?;
        Ok(res.json::<Res>().await?)
    }

    /// Executes a program without generating a proof.
    pub async fn execute(
        &self,
        program_id: impl Into<ProgramID>,
        input: Vec<u8>,
    ) -> Result<ExecuteResponse, Error> {
        self.post(
            "execute",
            ExecuteRequest {
                program_id: program_id.into(),
                input,
            },
        )
        .await
    }

    /// Generates a proof for a program execution.
    pub async fn prove(
        &self,
        program_id: impl Into<ProgramID>,
        input: Vec<u8>,
    ) -> Result<ProveResponse, Error> {
        self.post(
            "prove",
            ProveRequest {
                program_id: program_id.into(),
                input,
            },
        )
        .await
    }

    /// Verifies a proof without re-executing the program.
    pub async fn verify(
        &self,
        program_id: impl Into<ProgramID>,
        proof: Vec<u8>,
    ) -> Result<VerifyResponse, Error> {
        self.post(
            "verify",
            VerifyRequest {
                program_id: program_id.into(),
                proof,
            },
        )
        .await
    }

    /// Retrieves server hardware and system information.
    pub async fn info(&self) -> Result<ServerInfoResponse, Error> {
        self.get("info").await
    }

    /// Checks if the server is healthy and responsive.
    pub async fn health(&self) -> Result<(), Error> {
        send(self.client.get(self.base_url.join("health").unwrap())).await?;
        Ok(())
    }
}

/// Sends an HTTP request and handles error status codes.
pub(crate) async fn send(request: RequestBuilder) -> Result<Response, Error> {
    let res = request.send().await?;

    if let Err(inner) = res.error_for_status_ref() {
        let msg = res.text().await.ok();
        return Err(Error::ErrorStatus { inner, msg });
    }

    Ok(res)
}

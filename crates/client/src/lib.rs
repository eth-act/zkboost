//! HTTP client library for the zkboost Proof Node API.
//!
//! Provides [`zkBoostClient`] with methods for all four API operations:
//!
//! - [`request_proof`](zkBoostClient::request_proof) - submit a `NewPayloadRequest` for proving
//! - [`subscribe_proof_events`](zkBoostClient::subscribe_proof_events) - stream SSE proof
//!   completion/failure events
//! - [`get_proof`](zkBoostClient::get_proof) - download completed proof bytes
//! - [`verify_proof`](zkBoostClient::verify_proof) - verify a proof against the server
//!
//! # Example
//!
//! ```ignore
//! use zkboost_client::{zkBoostClient, MainnetEthSpec, NewPayloadRequest};
//! use zkboost_types::ProofType;
//!
//! # async fn example(request: NewPayloadRequest<MainnetEthSpec>) -> Result<(), Box<dyn std::error::Error>> {
//! let client = zkBoostClient::new("http://localhost:3000".parse()?);
//! let resp = client.request_proof(&request, &[ProofType::RethSP1]).await?;
//! println!("root: {:?}", resp.new_payload_request_root);
//! # Ok(())
//! # }
//! ```

#![warn(unused_crate_dependencies)]

pub mod error;

use bytes::Bytes;
use futures::stream::Stream;
use reqwest::{Response, StatusCode, header::CONTENT_TYPE};
use reqwest_eventsource::{Event, EventSource};
use serde::de::DeserializeOwned;
use tokio_stream::StreamExt;
use url::Url;

#[rustfmt::skip]
pub use {
    error::Error,
    zkboost_types::{
        Encode, FailureReason, Hash256, MainnetEthSpec,
        NewPayloadRequest, ProofComplete, ProofEvent, ProofFailure, ProofRequestResponse,
        ProofStatus, ProofType, ProofVerificationResponse,
        ProofEventParseError,
    },
};

const APPLICATION_OCTET_STREAM: &str = "application/octet-stream";

/// HTTP client for the zkboost Proof Node API.
#[derive(Debug, Clone)]
#[allow(non_camel_case_types)]
pub struct zkBoostClient {
    endpoint: Url,
    http_client: reqwest::Client,
}

impl zkBoostClient {
    /// Creates a new client pointing at the given base URL.
    pub fn new(endpoint: Url) -> Self {
        Self {
            endpoint,
            http_client: reqwest::Client::new(),
        }
    }

    /// Creates a new client with a custom [`reqwest::Client`].
    pub fn with_http_client(endpoint: Url, http_client: reqwest::Client) -> Self {
        Self {
            endpoint,
            http_client,
        }
    }

    /// Submit a [`NewPayloadRequest`] for proof generation.
    ///
    /// Sends `POST /v1/execution_proof_requests?proof_types=...` with the SSZ-encoded body. Returns
    /// the computed `new_payload_request_root` from the server.
    pub async fn request_proof(
        &self,
        new_payload_request: &NewPayloadRequest<MainnetEthSpec>,
        proof_types: &[ProofType],
    ) -> Result<ProofRequestResponse, Error> {
        let mut url = self.endpoint.join("/v1/execution_proof_requests")?;
        let proof_types = Vec::from_iter(proof_types.iter().map(ProofType::as_str)).join(",");
        url.query_pairs_mut()
            .append_pair("proof_types", &proof_types);

        let response = self
            .http_client
            .post(url)
            .header(CONTENT_TYPE, APPLICATION_OCTET_STREAM)
            .body(new_payload_request.as_ssz_bytes())
            .send()
            .await?;

        handle_json_response(response).await
    }

    /// Subscribe to SSE proof events.
    ///
    /// Opens `GET /v1/execution_proof_requests` as an SSE stream.
    ///
    /// When `filter_root` is provided, the server only sends events matching that
    /// `new_payload_request_root`.
    pub fn subscribe_proof_events(
        &self,
        filter_root: Option<Hash256>,
    ) -> impl Stream<Item = Result<ProofEvent, Error>> + Send + '_ {
        async_stream::try_stream! {
            let mut url = self.endpoint.join("/v1/execution_proof_requests")?;
            if let Some(new_payload_request_root) = filter_root {
                url.query_pairs_mut()
                    .append_pair("new_payload_request_root", &new_payload_request_root.to_string());
            }

            let builder = self.http_client.get(url);
            let mut es = EventSource::new(builder)
                .map_err(|e| Error::Sse(format!("failed to create event source: {e}")))?;

            while let Some(event) = es.next().await {
                match event {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(message)) => {
                        yield ProofEvent::try_from_parts(&message.event, &message.data)?;
                    }
                    Err(error) => {
                        es.close();
                        Err(Error::Sse(error.to_string()))?;
                    }
                }
            }
        }
    }

    /// Download a completed execution proof by proof type.
    ///
    /// Sends `GET /v1/execution_proofs/{root}/{proof_type}` and returns the raw proof bytes, or
    /// [`Error::NotFound`] if the proof is not yet available.
    pub async fn get_proof(
        &self,
        new_payload_request_root: Hash256,
        proof_type: ProofType,
    ) -> Result<Bytes, Error> {
        let url = self.endpoint.join(&format!(
            "/v1/execution_proofs/{new_payload_request_root}/{proof_type}"
        ))?;

        let response = error_for_status(self.http_client.get(url).send().await?).await?;
        Ok(response.bytes().await?)
    }

    /// Verify a proof against the server.
    ///
    /// Sends `POST /v1/execution_proof_verifications?new_payload_request_root=...&proof_type=...`
    /// with the raw proof bytes as the request body.
    pub async fn verify_proof(
        &self,
        new_payload_request_root: Hash256,
        proof_type: ProofType,
        proof: &[u8],
    ) -> Result<ProofVerificationResponse, Error> {
        let mut url = self.endpoint.join("/v1/execution_proof_verifications")?;
        url.query_pairs_mut()
            .append_pair(
                "new_payload_request_root",
                &new_payload_request_root.to_string(),
            )
            .append_pair("proof_type", proof_type.as_str());

        let response = self
            .http_client
            .post(url)
            .header(CONTENT_TYPE, APPLICATION_OCTET_STREAM)
            .body(proof.to_vec())
            .send()
            .await?;

        handle_json_response(response).await
    }
}

async fn error_for_status(response: Response) -> Result<Response, Error> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let raw_body = response.text().await.map_err(Error::Transport)?;
    let message = serde_json::from_str::<serde_json::Value>(&raw_body)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(String::from))
        .unwrap_or(raw_body);
    match status {
        StatusCode::NOT_FOUND => Err(Error::NotFound(message)),
        StatusCode::BAD_REQUEST => Err(Error::BadRequest(message)),
        _ => Err(Error::ServerError {
            status: status.as_u16(),
            body: message,
        }),
    }
}

async fn handle_json_response<T: DeserializeOwned>(response: Response) -> Result<T, Error> {
    let response = error_for_status(response).await?;
    Ok(response.json().await?)
}

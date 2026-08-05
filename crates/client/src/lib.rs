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
//! # Distributed tracing
//!
//! With the `otel` cargo feature enabled, every outbound request carries the current
//! `tracing` span's W3C trace context (`traceparent`/`tracestate`), injected via the global
//! OpenTelemetry propagator, so proofs requested from an instrumented caller join its
//! distributed trace. Without the feature no OpenTelemetry dependency is pulled in and requests
//! are sent unchanged.
//!
//! Note that the global propagator defaults to a no-op: the calling application must install
//! one, e.g.
//!
//! ```ignore
//! opentelemetry::global::set_text_map_propagator(
//!     opentelemetry_sdk::propagation::TraceContextPropagator::new(),
//! );
//! ```
//!
//! otherwise no `traceparent` header is emitted even with the feature enabled.
//!
//! # Example
//!
//! ```ignore
//! use zkboost_client::{zkBoostClient, NewPayloadRequest};
//! use zkboost_types::{ChainConfig, ProofType, ProtocolFork};
//!
//! # async fn example(fork: ProtocolFork, request: NewPayloadRequest, chain_config: ChainConfig) -> Result<(), Box<dyn std::error::Error>> {
//! let client = zkBoostClient::new("http://localhost:3000".parse()?);
//! let resp = client.request_proof(fork, &request, &chain_config, &[ProofType::RethSP1]).await?;
//! println!("root: {:?}", resp.new_payload_request_root);
//! # Ok(())
//! # }
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

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
        ChainConfig, FailureReason, Hash256,
        NewPayloadRequest, ProofComplete, ProofEvent, ProofFailure, ProofRequestBody,
        ProofRequestResponse, ProofStatus, ProofType, ProofVerificationBody,
        ProofVerificationResponse, ProofEventParseError, ProtocolFork, SszEncode,
    },
};

const APPLICATION_OCTET_STREAM: &str = "application/octet-stream";

/// [`Injector`](opentelemetry::propagation::Injector) over HTTP headers, used to inject W3C trace
/// context (`traceparent`/`tracestate`) into outbound requests.
#[cfg(feature = "otel")]
struct HeaderInjector<'a>(&'a mut reqwest::header::HeaderMap);

#[cfg(feature = "otel")]
impl opentelemetry::propagation::Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(key.as_bytes())
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&value)
        {
            self.0.insert(name, value);
        }
    }
}

/// Returns headers carrying the current `tracing` span's W3C trace context
/// (`traceparent`/`tracestate`), injected via the global OpenTelemetry propagator.
///
/// Without the `otel` feature this returns an empty map, so requests are sent unchanged. With
/// the feature, the map is still empty unless the application has installed a global text-map
/// propagator (the OpenTelemetry default is a no-op).
fn trace_context_headers() -> reqwest::header::HeaderMap {
    #[cfg_attr(not(feature = "otel"), expect(unused_mut))]
    let mut headers = reqwest::header::HeaderMap::new();
    #[cfg(feature = "otel")]
    {
        use tracing_opentelemetry::OpenTelemetrySpanExt;

        let context = tracing::Span::current().context();
        opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&context, &mut HeaderInjector(&mut headers));
        });
    }
    headers
}

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
    ///
    /// Use this to customize transport behavior, e.g. attach extra headers to every request via
    /// [`reqwest::ClientBuilder::default_headers`] (for authenticated endpoints).
    pub fn with_http_client(endpoint: Url, http_client: reqwest::Client) -> Self {
        Self {
            endpoint,
            http_client,
        }
    }

    /// Submit a [`NewPayloadRequest`] for proof generation.
    ///
    /// Sends `POST /v1/execution_proof_requests` with an SSZ-encoded [`ProofRequestBody`] carrying
    /// the fork, proof types, payload, and chain config. Returns the computed
    /// `new_payload_request_root` from the server.
    pub async fn request_proof(
        &self,
        fork: ProtocolFork,
        new_payload_request: &NewPayloadRequest,
        chain_config: &ChainConfig,
        proof_types: &[ProofType],
    ) -> Result<ProofRequestResponse, Error> {
        let url = self.endpoint.join("/v1/execution_proof_requests")?;
        let body = ProofRequestBody {
            fork,
            new_payload_request: new_payload_request.clone(),
            chain_config: chain_config.clone(),
            proof_types: proof_types.to_vec(),
        };

        let response = self
            .http_client
            .post(url)
            .headers(trace_context_headers())
            .header(CONTENT_TYPE, APPLICATION_OCTET_STREAM)
            .body(body.to_ssz())
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
        // Capture the trace context eagerly so the subscription joins the span current at call
        // time, not whichever span happens to be current when the stream is first polled.
        let trace_headers = trace_context_headers();
        async_stream::try_stream! {
            let mut url = self.endpoint.join("/v1/execution_proof_requests")?;
            if let Some(new_payload_request_root) = filter_root {
                url.query_pairs_mut()
                    .append_pair("new_payload_request_root", &new_payload_request_root.to_string());
            }

            let builder = self.http_client.get(url).headers(trace_headers);
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

        let request = self.http_client.get(url).headers(trace_context_headers());
        let response = error_for_status(request.send().await?).await?;
        Ok(response.bytes().await?)
    }

    /// Verify a proof against the server.
    ///
    /// Sends `POST /v1/execution_proof_verifications` with an SSZ-encoded [`ProofVerificationBody`]
    /// carrying the fork, root, chain config, proof type, and proof bytes.
    pub async fn verify_proof(
        &self,
        fork: ProtocolFork,
        new_payload_request_root: Hash256,
        chain_config: &ChainConfig,
        proof_type: ProofType,
        proof: &[u8],
    ) -> Result<ProofVerificationResponse, Error> {
        let url = self.endpoint.join("/v1/execution_proof_verifications")?;
        let body = ProofVerificationBody {
            fork,
            new_payload_request_root: new_payload_request_root.0,
            chain_config: chain_config.clone(),
            proof_type,
            proof: proof.to_vec(),
        };

        let response = self
            .http_client
            .post(url)
            .headers(trace_context_headers())
            .header(CONTENT_TYPE, APPLICATION_OCTET_STREAM)
            .body(body.to_ssz())
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

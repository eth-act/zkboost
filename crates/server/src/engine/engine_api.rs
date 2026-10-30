//! The Engine API JSON-RPC envelopes and the client of the EL engine endpoint, which forwards a
//! request body as is with the caller's `Authorization` header and returns the raw response.

use axum::http::{HeaderValue, StatusCode};
use bytes::Bytes;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, value::RawValue};
use url::Url;

/// Raw HTTP response of the EL engine endpoint.
#[derive(Debug)]
pub(crate) struct EngineResponse {
    /// HTTP status of the response.
    pub(crate) status: StatusCode,
    /// Body of the response.
    pub(crate) body: Bytes,
}

/// Client of the EL engine endpoint.
#[derive(Debug)]
pub(crate) struct EngineApiClient {
    url: Url,
    http_client: reqwest::Client,
}

impl EngineApiClient {
    /// Creates the client of the EL engine endpoint at the url.
    pub(crate) fn new(url: Url) -> Self {
        Self {
            url,
            http_client: reqwest::Client::new(),
        }
    }

    /// Forwards a JSON-RPC request body with the caller's `Authorization` header.
    pub(crate) async fn forward(
        &self,
        authorization: Option<&HeaderValue>,
        body: Bytes,
    ) -> reqwest::Result<EngineResponse> {
        let mut request = self
            .http_client
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization.as_bytes());
        }
        let response = request.send().await?;
        Ok(EngineResponse {
            status: response.status(),
            body: response.bytes().await?,
        })
    }
}

/// JSON-RPC request envelope.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct JsonRpcRequest {
    jsonrpc: Value,
    id: Value,
    /// Method of the request.
    pub(crate) method: String,
    /// Kept as raw JSON, so the request is encoded again without decoding them.
    pub(crate) params: Box<RawValue>,
}

impl JsonRpcRequest {
    /// Decodes a JSON-RPC request body, or returns `None` for any other body.
    pub(crate) fn decode(body: &[u8]) -> Option<Self> {
        serde_json::from_slice(body).ok()
    }

    /// Returns the request with the method replaced.
    pub(crate) fn with_method(self, method: &str) -> Self {
        Self {
            method: method.to_owned(),
            ..self
        }
    }
}

/// JSON-RPC response envelope.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct JsonRpcResponse<T> {
    jsonrpc: Value,
    id: Value,
    /// Result of a successful call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<T>,
    /// Error of a failed call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<JsonRpcError>,
}

/// JSON-RPC error object. Every field but the code passes through unchanged.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct JsonRpcError {
    /// Error code.
    pub(crate) code: i64,
    #[serde(flatten)]
    other: Map<String, Value>,
}

/// Status of a payload in the answer of `engine_newPayload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum NewPayloadStatus {
    /// The payload is valid.
    Valid,
    /// The payload is invalid.
    Invalid,
    /// The EL cannot validate the payload while it syncs.
    Syncing,
    /// The EL accepted the payload without a full validation.
    Accepted,
}

impl NewPayloadStatus {
    /// Returns the status as the EL sends it, which is also its metric label.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "VALID",
            Self::Invalid => "INVALID",
            Self::Syncing => "SYNCING",
            Self::Accepted => "ACCEPTED",
        }
    }
}

/// Result of `engine_newPayloadWithWitness`. The witness is not serialized, and every other field
/// passes through unchanged.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NewPayloadWithWitnessResponse {
    status: Value,
    /// RLP-encoded witness of a `VALID` payload.
    #[serde(skip_serializing)]
    pub(crate) witness: Option<alloy_primitives::Bytes>,
    #[serde(flatten)]
    other: Map<String, Value>,
}

impl JsonRpcResponse<NewPayloadWithWitnessResponse> {
    /// Returns the payload status of the answer, `None` for an error answer or an unknown status.
    pub(crate) fn status(&self) -> Option<NewPayloadStatus> {
        match (&self.error, &self.result) {
            (None, Some(result)) => NewPayloadStatus::deserialize(&result.status).ok(),
            _ => None,
        }
    }
}

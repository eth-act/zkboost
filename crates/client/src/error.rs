//! Error types for the zkboost client.

/// Errors that can occur when using [`crate::zkBoostClient`].
#[derive(Debug, thiserror::Error)]
#[allow(non_camel_case_types)]
pub enum Error {
    /// An HTTP request failed at the transport level.
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    /// The server returned a non-2xx status code.
    #[error("server returned {status}: {body}")]
    ServerError {
        /// HTTP status code.
        status: u16,
        /// Response body text.
        body: String,
    },

    /// The requested resource was not found (404).
    #[error("not found: {0}")]
    NotFound(String),

    /// The server returned a 400 Bad Request.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// A transport error occurred reading the response body.
    #[error("transport error reading response body: {0}")]
    Transport(reqwest::Error),

    /// An error occurred on the SSE stream.
    #[error("SSE error: {0}")]
    Sse(String),

    /// Failed to parse a JSON response.
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),

    /// Failed to parse an SSE event into a [`ProofEvent`](zkboost_types::ProofEvent).
    #[error("SSE event parse error: {0}")]
    EventParse(#[from] zkboost_types::ProofEventParseError),

    /// Failed to construct a URL.
    #[error("URL error: {0}")]
    Url(#[from] url::ParseError),
}

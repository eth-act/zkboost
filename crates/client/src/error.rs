use thiserror::Error;

/// Error type for zkboost client operations.
#[derive(Debug, Error)]
pub enum Error {
    /// HTTP request failed or URL conversion failed.
    #[error("Request error: {0}")]
    Reqwest(#[from] reqwest::Error),
    /// Server returned an error response with status code >= 400.
    #[error("Requested {} failed with status {} and msg {}", inner.url().unwrap(), inner.status().unwrap(), msg.as_deref().unwrap_or("Unknown"))]
    ErrorStatus {
        /// The underlying HTTP error containing status code and URL.
        inner: reqwest::Error,
        /// Error message from the server response body, if available.
        msg: Option<String>,
    },
}

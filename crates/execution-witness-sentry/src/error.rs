//! Error types for the execution witness sentry.

use std::io;

use thiserror::Error;

/// Errors that can occur in the execution witness sentry.
#[derive(Debug, Error)]
pub enum Error {
    /// Failed to load or parse configuration.
    #[error("config error: {0}")]
    Config(String),

    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON-RPC error returned by the node.
    #[error("RPC error {code}: {message}")]
    Rpc {
        /// Error code.
        code: i64,
        /// Error message.
        message: String,
    },

    /// Failed to parse response.
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),

    /// WebSocket connection or subscription failed.
    #[error("WebSocket error: {0}")]
    WebSocket(String),

    /// URL parsing failed.
    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    /// I/O error (file operations, compression).
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// TOML parsing error.
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
}

/// Result type alias using our Error type.
pub type Result<T> = std::result::Result<T, Error>;

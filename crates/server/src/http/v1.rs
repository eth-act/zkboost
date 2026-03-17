//! v1 API handlers:
//!
//! - `POST /execution_proof_requests`
//! - `GET /execution_proof_requests` (SSE)
//! - `GET /execution_proofs/{new_payload_request_root}/{type}`
//! - `POST /execution_proof_verifications`

use axum::{Json, http::StatusCode};

mod get_execution_proof_requests;
mod get_execution_proofs;
mod post_execution_proof_requests;
mod post_execution_proof_verifications;

pub(crate) use get_execution_proof_requests::get_execution_proof_requests;
pub(crate) use get_execution_proofs::get_execution_proofs;
pub(crate) use post_execution_proof_requests::post_execution_proof_requests;
pub(crate) use post_execution_proof_verifications::post_execution_proof_verifications;

/// JSON error response body returned by API endpoints.
#[derive(Debug, serde::Serialize)]
pub(crate) struct ErrorResponse {
    pub(crate) error: String,
}

/// Constructs an error response tuple for Axum handlers.
pub(crate) fn error_response(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
}

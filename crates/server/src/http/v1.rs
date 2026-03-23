//! v1 API handlers:
//!
//! - `POST /execution_proof_requests`
//! - `GET /execution_proof_requests` (SSE)
//! - `GET /execution_proofs/{new_payload_request_root}/{type}`
//! - `POST /execution_proof_verifications`

use axum::{
    Json,
    extract::FromRequestParts,
    http::{StatusCode, request::Parts},
    response::IntoResponse,
};
use serde::de::DeserializeOwned;

mod get_execution_proof_requests;
mod get_execution_proofs;
mod post_execution_proof_requests;
mod post_execution_proof_verifications;

pub(crate) use get_execution_proof_requests::get_execution_proof_requests;
pub(crate) use get_execution_proofs::get_execution_proofs;
pub(crate) use post_execution_proof_requests::post_execution_proof_requests;
pub(crate) use post_execution_proof_verifications::post_execution_proof_verifications;

/// JSON error response body returned by API endpoints, following the beacon-API convention.
#[derive(Debug)]
pub(crate) struct ErrorResponse {
    /// HTTP status code.
    code: StatusCode,
    /// Human-readable error message.
    message: String,
}

impl ErrorResponse {
    pub(crate) fn new(code: StatusCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub(crate) fn internal_server_error(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> axum::response::Response {
        #[derive(serde::Serialize)]
        struct Body {
            code: u16,
            message: String,
        }

        (
            self.code,
            Json(Body {
                code: self.code.as_u16(),
                message: self.message,
            }),
        )
            .into_response()
    }
}

/// Wrapper around [`axum::extract::Query`] that returns a JSON [`ErrorResponse`] on deserialization
/// failure.
#[derive(Debug)]
pub(crate) struct Query<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for Query<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ErrorResponse;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Query(value)),
            Err(rejection) => Err(ErrorResponse::new(
                rejection.status(),
                rejection.body_text(),
            )),
        }
    }
}

/// Wrapper around [`axum::extract::Path`] that returns a JSON [`ErrorResponse`] on deserialization
/// failure.
#[derive(Debug)]
pub(crate) struct Path<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for Path<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ErrorResponse;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(Path(value)),
            Err(rejection) => Err(ErrorResponse::new(
                rejection.status(),
                rejection.body_text(),
            )),
        }
    }
}

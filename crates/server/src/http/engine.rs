//! Engine API handler for `POST /`, see [`crate::engine`].

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use tracing::{error, instrument};

use crate::{
    engine::{EngineResponse, NewPayload},
    http::AppState,
};

/// Forwards an Engine API JSON-RPC request to the EL, through the proving pipeline for a new
/// payload. A request that cannot reach the EL is answered with 502.
#[instrument(skip_all)]
pub(crate) async fn post_engine(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let authorization = headers.get(AUTHORIZATION);
    let upstream = match NewPayload::decode(&body) {
        Some(new_payload) => {
            state
                .engine
                .clone()
                .new_payload(authorization, new_payload, body)
                .await
        }
        None => state.engine.forward(authorization, body).await,
    };
    match upstream {
        Ok(EngineResponse { status, body }) => {
            (status, [("content-type", "application/json")], body).into_response()
        }
        Err(error) => {
            error!(%error, "engine request forward failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use crate::http::{router, tests::mock_app_state};

    /// A request that cannot reach the EL is answered with 502 instead of a JSON-RPC error
    /// forged on the EL's behalf.
    #[tokio::test]
    async fn test_unreachable_el_returns_bad_gateway() {
        let state = mock_app_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"engine_exchangeCapabilities","params":[[]]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 502);
    }

    /// Unknown routes fall through to the default 404.
    #[tokio::test]
    async fn test_unknown_route_returns_404() {
        let state = mock_app_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }
}

//! Engine API handler for `POST /`, see [`crate::engine`].

use std::sync::Arc;

use axum::{
    Extension,
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use tracing::{error, instrument};

use crate::{
    engine::{JsonRpcRequest, NewPayload, engine_api_client::EngineResponse},
    http::AppState,
    metrics::JsonRpcMethod,
};

/// Forwards an Engine API JSON-RPC request to the EL, through the proving pipeline for a new
/// payload. A request that cannot reach the EL is answered with 502. Every answer except a JWT
/// rejection of the EL carries the [`JsonRpcMethod`] of the request for the HTTP metrics.
#[instrument(skip_all)]
pub(crate) async fn post_engine(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let authorization = headers.get(AUTHORIZATION);
    let request = JsonRpcRequest::decode(&body);
    let json_rpc_method = request
        .as_ref()
        .map(|request| Extension(JsonRpcMethod(request.method.clone())));
    let upstream = match request.and_then(NewPayload::decode) {
        Some(new_payload) => {
            state
                .engine
                .clone()
                .new_payload(authorization, new_payload, body)
                .await
        }
        None => {
            state
                .engine
                .engine_api_client
                .forward(authorization, body)
                .await
        }
    };
    match upstream {
        Ok(EngineResponse { status, body }) => (
            status,
            json_rpc_method
                .filter(|_| !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)),
            [("content-type", "application/json")],
            body,
        )
            .into_response(),
        Err(error) => {
            error!(%error, "engine request forward failed");
            (StatusCode::BAD_GATEWAY, json_rpc_method, ()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use metrics::set_default_local_recorder;
    use tower::ServiceExt;

    use crate::{
        http::{router, tests::mock_app_state},
        metrics::prometheus_builder,
    };

    /// A request that cannot reach the EL is answered with 502 instead of a JSON-RPC error
    /// forged on the EL's behalf, and recorded under its method.
    #[tokio::test]
    async fn test_unreachable_el_returns_bad_gateway() {
        let recorder = prometheus_builder().build_recorder();
        let _recorder_guard = set_default_local_recorder(&recorder);
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
        assert!(recorder.handle().render().contains(
            r#"zkboost_http_requests_total{endpoint="engine_exchangeCapabilities",method="POST",status="502"} 1"#
        ));
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

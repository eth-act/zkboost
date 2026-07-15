//! Tests that outbound client requests carry the calling span's W3C trace context
//! (`traceparent`) when the `otel` feature is enabled.

use std::sync::{Arc, Mutex};

use axum::{Router, extract::State, http::HeaderMap};
use opentelemetry::trace::{TraceContextExt, TracerProvider};
use opentelemetry_sdk::{propagation::TraceContextPropagator, trace::SdkTracerProvider};
use tokio_stream::StreamExt;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;
use zkboost_client::{Hash256, ProofType, zkBoostClient};

/// `traceparent` header values captured by the local server, one per request received.
type CapturedTraceparents = Arc<Mutex<Vec<Option<String>>>>;

async fn capture_traceparent(State(captured): State<CapturedTraceparents>, headers: HeaderMap) {
    let traceparent = headers
        .get("traceparent")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    captured.lock().unwrap().push(traceparent);
}

/// Extracts the trace id from a W3C `traceparent` header (`00-<trace_id>-<span_id>-<flags>`).
fn trace_id_of(traceparent: &str) -> &str {
    traceparent
        .split('-')
        .nth(1)
        .expect("traceparent should have a trace id field")
}

/// Sends requests from within tracing spans and asserts each request arrives with a
/// `traceparent` header carrying the active span's trace id.
#[tokio::test]
async fn test_outbound_requests_carry_active_span_trace_context() {
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let provider = SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::OpenTelemetryLayer::new(provider.tracer("test")),
    );
    let _guard = tracing::subscriber::set_default(subscriber);

    // Local server capturing the `traceparent` header of every request.
    let captured = CapturedTraceparents::default();
    let app = Router::new()
        .fallback(capture_traceparent)
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = zkBoostClient::new(endpoint.parse().unwrap());

    // Unary request: `get_proof` awaited inside a span.
    let unary_span = tracing::info_span!("unary_caller");
    let unary_trace_id = unary_span
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    assert_ne!(
        unary_trace_id, "00000000000000000000000000000000",
        "span should carry a valid OpenTelemetry context"
    );
    client
        .get_proof(Hash256::ZERO, ProofType::RethZisk)
        .instrument(unary_span)
        .await
        .unwrap();

    // SSE subscription: the context is captured when `subscribe_proof_events` is called, even
    // though the request is only sent once the stream is polled (outside the span, here). The
    // plain 200 response fails SSE content-type validation, which is fine: the request has
    // already reached the server with its headers by then.
    let sse_span = tracing::info_span!("sse_caller");
    let sse_trace_id = sse_span
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    let stream = {
        let _entered = sse_span.enter();
        client.subscribe_proof_events(None)
    };
    let mut stream = std::pin::pin!(stream);
    let _ = stream.next().await;

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2);
    let unary_traceparent = captured[0]
        .as_deref()
        .expect("get_proof should send traceparent");
    assert_eq!(trace_id_of(unary_traceparent), unary_trace_id);
    let sse_traceparent = captured[1]
        .as_deref()
        .expect("subscribe should send traceparent");
    assert_eq!(trace_id_of(sse_traceparent), sse_trace_id);
}

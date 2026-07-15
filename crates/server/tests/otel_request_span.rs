//! Verifies that the server's `request` span joins a caller-supplied W3C trace context
//! (`traceparent`) and carries the expected OpenTelemetry span kind and HTTP attributes.
//!
//! This lives in its own integration-test binary on purpose: it installs a process-global
//! tracing subscriber so spans created on the server's tasks are observed. Sharing a process
//! with other tests would both race on the global subscriber and poison the global callsite
//! interest cache, which previously required warm-up/retry workarounds.

use std::{collections::HashMap, time::Duration};

use metrics_exporter_prometheus::PrometheusBuilder;
use opentelemetry::trace::{SpanKind, TracerProvider};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{InMemorySpanExporter, SdkTracerProvider},
};
use tracing_subscriber::layer::SubscriberExt;
use zkboost_server::{
    config::{Config, DashboardConfig, MockProvingTime, zkVMConfig},
    server::zkBoostServer,
};
use zkboost_types::ProofType;

/// Sends a request carrying a W3C `traceparent` header and asserts the exported `request`
/// span joins the caller's trace: same trace id, parented under the caller's span id.
#[tokio::test]
async fn test_request_span_joins_remote_trace_context() {
    const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";
    const PARENT_SPAN_ID: &str = "b7ad6b7169203331";

    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::OpenTelemetryLayer::new(provider.tracer("test")),
    );
    tracing::subscriber::set_global_default(subscriber)
        .expect("no other subscriber should be installed in this test binary");

    // A local chain config file lets the server start without contacting the EL endpoint.
    let config = Config {
        port: 0,
        el_endpoint: "http://127.0.0.1:1/".parse().unwrap(),
        el_headers: HashMap::new(),
        chain_config_path: Some(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixture/chain_config.json"
            )
            .into(),
        ),
        witness_timeout_secs: 12,
        proof_cache_size: 128,
        witness_cache_size: 128,
        dashboard: DashboardConfig::default(),
        zkvm: vec![zkVMConfig::Mock {
            proof_type: ProofType::RethZisk,
            proof_timeout_secs: 12,
            mock_proving_time: MockProvingTime::Constant { ms: 10 },
            mock_proof_size: 64,
            mock_failure: false,
        }],
    };
    let metrics = PrometheusBuilder::new().build_recorder().handle();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let server = zkBoostServer::new(config, metrics).await.unwrap();
    let (addr, _handles) = server.run(shutdown.clone()).await.unwrap();

    let response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{}/v1/proof_types", addr.port()))
        .header("traceparent", format!("00-{TRACE_ID}-{PARENT_SPAN_ID}-01"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    // The request span stays open until the response body is fully sent.
    response.text().await.unwrap();

    // The server closes the span shortly after the client finishes reading the body; poll
    // briefly for the exported span rather than racing that completion.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let request_span = loop {
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        if let Some(span) = spans.iter().find(|span| span.name == "request") {
            break span.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "request span should be exported"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert_eq!(request_span.span_context.trace_id().to_string(), TRACE_ID);
    assert_eq!(request_span.parent_span_id.to_string(), PARENT_SPAN_ID);
    assert_eq!(request_span.span_kind, SpanKind::Server);
    let attribute = |key: &str| {
        request_span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.as_str().into_owned())
    };
    assert_eq!(attribute("http.request.method").as_deref(), Some("GET"));
    assert_eq!(attribute("url.path").as_deref(), Some("/v1/proof_types"));

    shutdown.cancel();
}

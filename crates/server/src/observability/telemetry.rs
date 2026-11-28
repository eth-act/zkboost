//! Logging, and tracing to an OTLP endpoint when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.

use std::env;

use axum::http::HeaderMap;
use opentelemetry::{global, propagation::Extractor, trace::TracerProvider};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{Resource, propagation::TraceContextPropagator, trace::SdkTracerProvider};
use tracing::Span;
use tracing_opentelemetry::{OpenTelemetryLayer, OpenTelemetrySpanExt};
use tracing_subscriber::{
    EnvFilter,
    field::MakeExt,
    fmt::{self, format::debug_fn},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

/// Flushes the pending spans when it drops.
pub struct Telemetry(Option<SdkTracerProvider>);

/// Installs the global subscriber, which logs under the `RUST_LOG` filter.
pub fn init() -> Telemetry {
    let provider = env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .map(|endpoint| {
            global::set_text_map_propagator(TraceContextPropagator::new());
            let exporter = SpanExporter::builder().with_tonic().with_endpoint(endpoint);
            let service_name =
                env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "zkboost".to_owned());
            SdkTracerProvider::builder()
                .with_batch_exporter(exporter.build().expect("the otlp exporter builds"))
                .with_resource(Resource::builder().with_service_name(service_name).build())
                .build()
        });
    // The `otel.*` fields configure the exported span and are no context of a log line.
    let log_fields = debug_fn(|writer, field, value| match field.name() {
        name if name.starts_with("otel.") => Ok(()),
        "message" => write!(writer, "{value:?}"),
        name => write!(writer, "{name}={value:?}"),
    })
    .delimited(" ");
    tracing_subscriber::registry()
        .with(
            (provider.as_ref()).map(|provider| OpenTelemetryLayer::new(provider.tracer("zkboost"))),
        )
        .with(fmt::layer().compact().fmt_fields(log_fields))
        .with(EnvFilter::from_default_env())
        .init();
    Telemetry(provider)
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(Err(error)) = self.0.as_ref().map(SdkTracerProvider::shutdown) {
            eprintln!("otel provider shutdown failed: {error}");
        }
    }
}

/// Makes the W3C trace context of the request headers the parent of the span, so the span joins
/// the trace of the caller.
pub(crate) fn set_remote_parent(span: &Span, headers: &HeaderMap) {
    struct Headers<'a>(&'a HeaderMap);

    impl Extractor for Headers<'_> {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key)?.to_str().ok()
        }

        fn keys(&self) -> Vec<&str> {
            self.0.keys().map(|key| key.as_str()).collect()
        }
    }

    let parent =
        global::get_text_map_propagator(|propagator| propagator.extract(&Headers(headers)));
    // This fails only without the OpenTelemetry layer, when there is no trace to join.
    let _ = span.set_parent(parent);
}

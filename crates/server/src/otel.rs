//! OpenTelemetry telemetry initialization for distributed tracing via OTLP/gRPC.

use std::env;

use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
    propagation::TraceContextPropagator,
    trace::{SdkTracer, SdkTracerProvider},
};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::Registry;

/// Type alias for the OpenTelemetry tracing layer.
pub type OtelLayer = OpenTelemetryLayer<Registry, SdkTracer>;

/// Initializes OpenTelemetry tracing if `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Returns a provider
/// handle for explicit shutdown and an optional layer to attach to the tracing subscriber.
pub fn init() -> (Option<SdkTracerProvider>, Option<OtelLayer>) {
    let service_name = env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "zkboost".to_owned());
    let otel_endpoint = env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();

    let provider = otel_endpoint.map(|endpoint| {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        let exporter = SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .expect("failed to create OTLP exporter");
        let resource = Resource::builder()
            .with_service_name(service_name.clone())
            .build();
        SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource)
            .build()
    });

    let otel_layer = provider
        .as_ref()
        .map(|p| OpenTelemetryLayer::new(p.tracer(service_name)));

    (provider, otel_layer)
}

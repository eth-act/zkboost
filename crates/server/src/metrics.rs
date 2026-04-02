//! Prometheus metrics registration, recording helpers, and HTTP middleware.

use std::{
    array::from_fn,
    time::{Duration, Instant},
};

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use zkboost_types::ProofType;

const DEFAULT_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Initialize the Prometheus metrics exporter and register metric descriptions.
///
/// Returns a handle that can be used to render metrics for the `/metrics` endpoint.
pub fn init_metrics() -> PrometheusHandle {
    let handle = PrometheusBuilder::new()
        .set_buckets(DEFAULT_BUCKETS)
        .unwrap()
        .set_buckets_for_metric(
            Matcher::Full("zkboost_prove_duration_seconds".to_owned()),
            &from_fn::<_, 24, _>(|i| (i + 1) as f64 * 0.5),
        )
        .unwrap()
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    // HTTP layer metrics
    describe_counter!("zkboost_http_requests_total", "total http requests");
    describe_histogram!(
        "zkboost_http_request_duration_seconds",
        "http request duration"
    );
    describe_gauge!("zkboost_http_requests_in_flight", "http requests in flight");

    // Prove operation metrics
    describe_counter!("zkboost_prove_total", "total prove operations");
    describe_histogram!(
        "zkboost_prove_duration_seconds",
        "proof generation duration"
    );
    describe_histogram!("zkboost_prove_proof_bytes", "proof size");

    // Verify operation metrics
    describe_counter!("zkboost_verify_total", "total verify operations");
    describe_histogram!(
        "zkboost_verify_duration_seconds",
        "proof verification duration"
    );

    // Application metrics
    describe_gauge!("zkboost_programs_loaded", "zkvm programs loaded");
    describe_gauge!("zkboost_build_info", "build info");

    handle
}

/// Spawn a background task that calls `run_upkeep()` every 5 seconds.
pub fn spawn_upkeep(handle: PrometheusHandle) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            handle.run_upkeep();
        }
    });
}

/// Record an HTTP request start (increment in-flight gauge).
fn record_request_start(endpoint: &str) {
    gauge!("zkboost_http_requests_in_flight", "endpoint" => endpoint.to_owned()).increment(1.0);
}

/// Record an HTTP request completion with status and duration.
fn record_request_end(endpoint: &str, method: &str, status: u16, duration: Duration) {
    let endpoint = endpoint.to_owned();
    let method = method.to_owned();
    gauge!("zkboost_http_requests_in_flight", "endpoint" => endpoint.clone()).decrement(1.0);
    counter!(
        "zkboost_http_requests_total",
        "endpoint" => endpoint.clone(),
        "method" => method.clone(),
        "status" => status.to_string()
    )
    .increment(1);
    histogram!(
        "zkboost_http_request_duration_seconds",
        "endpoint" => endpoint,
        "method" => method
    )
    .record(duration.as_secs_f64());
}

/// Record a prove operation result.
pub fn record_prove(
    proof_type: ProofType,
    status: &'static str,
    duration: Duration,
    proof_size: usize,
) {
    counter!(
        "zkboost_prove_total",
        "proof_type" => proof_type.to_string(),
        "status" => status
    )
    .increment(1);
    if status == "success" {
        histogram!(
            "zkboost_prove_duration_seconds",
            "proof_type" => proof_type.to_string(),
        )
        .record(duration.as_secs_f64());
        histogram!(
            "zkboost_prove_proof_bytes",
            "proof_type" => proof_type.to_string(),
        )
        .record(proof_size as f64);
    }
}

/// Record a verify operation result.
pub fn record_verify(proof_type: ProofType, verified: bool, duration: Duration) {
    counter!(
        "zkboost_verify_total",
        "proof_type" => proof_type.to_string(),
        "verified" => verified.to_string()
    )
    .increment(1);
    histogram!(
        "zkboost_verify_duration_seconds",
        "proof_type" => proof_type.to_string(),
    )
    .record(duration.as_secs_f64());
}

/// Set the number of loaded programs gauge.
pub fn set_programs_loaded(count: usize) {
    gauge!("zkboost_programs_loaded").set(count as f64);
}

/// Set the build info gauge with version label.
pub fn set_build_info(version: &str) {
    gauge!("zkboost_build_info", "version" => version.to_string()).set(1.0);
}

/// Axum middleware that records HTTP request metrics.
///
/// Uses `MatchedPath` (the route template) rather than the raw URI to avoid
/// unbounded metric cardinality from path parameters.
pub(crate) async fn http_metrics_middleware(request: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = request.method().to_string();
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|mp| mp.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());

    record_request_start(&path);

    let response = next.run(request).await;

    let status = response.status().as_u16();
    record_request_end(&path, &method, status, start.elapsed());

    response
}

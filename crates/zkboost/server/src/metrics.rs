//! Prometheus metrics for the zkboost server.
//!
//! Provides metric initialization and helper functions for recording HTTP and zkVM operation
//! metrics.

use std::time::{Duration, Instant};

use axum::{extract::Request, middleware::Next, response::Response};
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Initialize the Prometheus metrics exporter and register metric descriptions.
///
/// Returns a handle that can be used to render metrics for the `/metrics` endpoint.
pub(crate) fn init_metrics() -> PrometheusHandle {
    let builder = PrometheusBuilder::new();
    let handle = builder
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    // HTTP layer metrics
    describe_counter!("zkboost_http_requests_total", "Total HTTP requests");
    describe_histogram!(
        "zkboost_http_request_duration_seconds",
        "HTTP request duration in seconds"
    );
    describe_gauge!(
        "zkboost_http_requests_in_flight",
        "Number of HTTP requests currently being processed"
    );

    // Prove operation metrics
    describe_counter!("zkboost_prove_total", "Total prove operations");
    describe_histogram!(
        "zkboost_prove_duration_seconds",
        "Proof generation time in seconds"
    );
    describe_histogram!(
        "zkboost_prove_proof_bytes",
        "Generated proof sizes in bytes"
    );

    // Execute operation metrics
    describe_counter!("zkboost_execute_total", "Total execute operations");
    describe_histogram!(
        "zkboost_execute_duration_seconds",
        "Program execution time in seconds"
    );
    describe_histogram!(
        "zkboost_execute_cycles_total",
        "Total cycle counts per execution"
    );

    // Verify operation metrics
    describe_counter!("zkboost_verify_total", "Total verify operations");
    describe_histogram!(
        "zkboost_verify_duration_seconds",
        "Proof verification time in seconds"
    );

    // Application metrics
    describe_gauge!(
        "zkboost_programs_loaded",
        "Number of zkVM programs currently loaded"
    );
    describe_gauge!("zkboost_build_info", "Build information");

    handle
}

/// Record an HTTP request start (increment in-flight gauge).
pub(crate) fn record_request_start(endpoint: &str) {
    gauge!("zkboost_http_requests_in_flight", "endpoint" => endpoint.to_string()).increment(1.0);
}

/// Record an HTTP request completion with status and duration.
pub(crate) fn record_request_end(endpoint: &str, method: &str, status: u16, duration: Duration) {
    gauge!("zkboost_http_requests_in_flight", "endpoint" => endpoint.to_string()).decrement(1.0);
    counter!(
        "zkboost_http_requests_total",
        "endpoint" => endpoint.to_string(),
        "method" => method.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
    histogram!(
        "zkboost_http_request_duration_seconds",
        "endpoint" => endpoint.to_string(),
        "method" => method.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Record a prove operation result.
pub(crate) fn record_prove(program_id: &str, success: bool, duration: Duration, proof_size: usize) {
    let status = if success { "success" } else { "error" };
    counter!(
        "zkboost_prove_total",
        "program_id" => program_id.to_string(),
        "status" => status
    )
    .increment(1);
    if success {
        histogram!(
            "zkboost_prove_duration_seconds",
            "program_id" => program_id.to_string()
        )
        .record(duration.as_secs_f64());
        histogram!(
            "zkboost_prove_proof_bytes",
            "program_id" => program_id.to_string()
        )
        .record(proof_size as f64);
    }
}

/// Record an execute operation result.
pub(crate) fn record_execute(program_id: &str, success: bool, duration: Duration, cycles: u64) {
    let status = if success { "success" } else { "error" };
    counter!(
        "zkboost_execute_total",
        "program_id" => program_id.to_string(),
        "status" => status
    )
    .increment(1);
    if success {
        histogram!(
            "zkboost_execute_duration_seconds",
            "program_id" => program_id.to_string()
        )
        .record(duration.as_secs_f64());
        histogram!(
            "zkboost_execute_cycles_total",
            "program_id" => program_id.to_string()
        )
        .record(cycles as f64);
    }
}

/// Record a verify operation result.
pub(crate) fn record_verify(program_id: &str, verified: bool, duration: Duration) {
    counter!(
        "zkboost_verify_total",
        "program_id" => program_id.to_string(),
        "verified" => verified.to_string()
    )
    .increment(1);
    histogram!(
        "zkboost_verify_duration_seconds",
        "program_id" => program_id.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Set the number of loaded programs gauge.
pub(crate) fn set_programs_loaded(count: usize) {
    gauge!("zkboost_programs_loaded").set(count as f64);
}

/// Set the build info gauge with version label.
pub(crate) fn set_build_info(version: &str) {
    gauge!("zkboost_build_info", "version" => version.to_string()).set(1.0);
}

/// Axum middleware that records HTTP request metrics.
pub(crate) async fn http_metrics_middleware(request: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = request.method().to_string();
    let path = request.uri().path().to_string();

    record_request_start(&path);

    let response = next.run(request).await;

    let status = response.status().as_u16();
    record_request_end(&path, &method, status, start.elapsed());

    response
}

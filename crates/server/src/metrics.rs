//! Prometheus metrics registration, recording helpers, and HTTP middleware.

use std::{
    array::from_fn,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use zkboost_types::ProofType;

const HTTP_REQUESTS_TOTAL: &str = "zkboost_http_requests_total";
const HTTP_REQUEST_DURATION_SECONDS: &str = "zkboost_http_request_duration_seconds";
const HTTP_REQUESTS_IN_FLIGHT: &str = "zkboost_http_requests_in_flight";
const WITNESS_FETCH_TOTAL: &str = "zkboost_witness_fetch_total";
const WITNESS_FETCH_DURATION_SECONDS: &str = "zkboost_witness_fetch_duration_seconds";
const WITNESS_BYTES: &str = "zkboost_witness_bytes";
const QUEUE_WAIT_DURATION_SECONDS: &str = "zkboost_queue_wait_duration_seconds";
const PROVE_TOTAL: &str = "zkboost_prove_total";
const PROVE_DURATION_SECONDS: &str = "zkboost_prove_duration_seconds";
const PROVE_PROOF_BYTES: &str = "zkboost_prove_proof_bytes";
const PROOF_REUSED_TOTAL: &str = "zkboost_proof_reused_total";
const SUBMISSION_TOTAL: &str = "zkboost_submission_total";
const SUBMISSION_DURATION_SECONDS: &str = "zkboost_submission_duration_seconds";
const PROOF_LATENCY_SECONDS: &str = "zkboost_proof_latency_seconds";
const PROGRAMS_LOADED: &str = "zkboost_programs_loaded";
const BUILD_INFO: &str = "zkboost_build_info";
const PAYLOAD_ARRIVAL_DELAY_SECONDS: &str = "zkboost_payload_arrival_delay_seconds";
const LATEST_PAYLOAD_TIMESTAMP_SECONDS: &str = "zkboost_latest_payload_timestamp_seconds";
const NEW_PAYLOAD_TOTAL: &str = "zkboost_new_payload_total";
const PROOF_DISPATCH_FAILURES_TOTAL: &str = "zkboost_proof_dispatch_failures_total";
const QUEUE_DEPTH: &str = "zkboost_queue_depth";
const PROOF_SLOT_LATENCY_SECONDS: &str = "zkboost_proof_slot_latency_seconds";
const VALIDATOR_READY: &str = "zkboost_validator_ready";

const DEFAULT_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

// Queue wait is unbounded (nothing caps how long an input sits in the worker
// channel), so unlike the prove buckets these keep a long tail: backlogs of
// minutes are exactly what this metric exists to expose. The 1 ms bucket holds the wait of an
// idle worker, which is no wait.
const QUEUE_WAIT_BUCKETS: &[f64] = &[
    0.001, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 900.0,
];

// The buckets past the slot, up to an hour, so a delay of minutes keeps its size instead of
// landing in +Inf.
const LATE_TAIL_BUCKETS: [f64; 9] = [24.0, 36.0, 60.0, 120.0, 180.0, 300.0, 600.0, 1800.0, 3600.0];

// The known label values of the counters. Every counter and histogram with labels known at startup
// starts at 0, because `increase` and `rate` do not count the first event of a series that starts
// after them.
const NEW_PAYLOAD_STATUSES: [&str; 6] =
    ["VALID", "INVALID", "SYNCING", "ACCEPTED", "other", "error"];
const WITNESS_FETCH_STATUSES: [&str; 4] = ["success", "missing", "not_valid", "error"];
const PROVE_STATUSES: [&str; 3] = ["success", "error", "timeout"];
const SUBMISSION_STATUSES: [&str; 2] = ["success", "error"];
const PROOF_DISPATCH_FAILURE_REASONS: [&str; 5] = [
    "input_error",
    "missing_witness",
    "unsupported_fork",
    "channel_full",
    "channel_closed",
];

/// Initializes the Prometheus metrics exporter and registers metric descriptions.
///
/// Returns a handle that can be used to render metrics for the `/metrics` endpoint.
pub fn init_metrics() -> PrometheusHandle {
    let handle = prometheus_builder()
        .install_recorder()
        .expect("failed to install Prometheus recorder");

    // HTTP layer metrics
    describe_counter!(
        HTTP_REQUESTS_TOTAL,
        "engine api requests by json-rpc method"
    );
    describe_histogram!(
        HTTP_REQUEST_DURATION_SECONDS,
        "engine api request duration by json-rpc method"
    );
    describe_gauge!(HTTP_REQUESTS_IN_FLIGHT, "http requests in flight");

    // Payload metrics
    describe_histogram!(
        PAYLOAD_ARRIVAL_DELAY_SECONDS,
        "time from the start of the slot of a payload to its engine_newPayload receipt"
    );
    describe_gauge!(
        LATEST_PAYLOAD_TIMESTAMP_SECONDS,
        "timestamp of the newest VALID payload, the start of its slot"
    );
    describe_counter!(
        NEW_PAYLOAD_TOTAL,
        "engine_newPayload requests by the status of the el answer"
    );

    // Witness operation metrics
    describe_counter!(WITNESS_FETCH_TOTAL, "total witness fetch operations");
    describe_histogram!(WITNESS_FETCH_DURATION_SECONDS, "witness fetch duration");
    describe_histogram!(WITNESS_BYTES, "witness size");

    // Prove operation metrics
    describe_counter!(
        PROOF_DISPATCH_FAILURES_TOTAL,
        "proof requests that end before their input reaches the worker"
    );
    describe_gauge!(
        QUEUE_DEPTH,
        "proof inputs waiting in the worker channel, without the input being proven"
    );
    describe_histogram!(
        QUEUE_WAIT_DURATION_SECONDS,
        "time a proof request waits in the worker channel between dispatch and dequeue"
    );
    describe_counter!(PROVE_TOTAL, "total prove operations");
    describe_histogram!(PROVE_DURATION_SECONDS, "proof generation duration");
    describe_histogram!(PROVE_PROOF_BYTES, "proof size");

    // Submission metrics
    describe_counter!(
        PROOF_REUSED_TOTAL,
        "cached proofs submitted again for a payload imported again"
    );
    describe_counter!(
        SUBMISSION_TOTAL,
        "total proof submissions to the beacon node"
    );
    describe_histogram!(
        SUBMISSION_DURATION_SECONDS,
        "proof submission duration, with the beacon block root lookup and the retries"
    );
    describe_histogram!(
        PROOF_LATENCY_SECONDS,
        "time from the payload receipt to the accepted proof"
    );
    describe_histogram!(
        PROOF_SLOT_LATENCY_SECONDS,
        "time from the start of the slot of the payload to the accepted proof"
    );

    // Application metrics
    describe_gauge!(PROGRAMS_LOADED, "zkvm programs loaded");
    describe_gauge!(BUILD_INFO, "build info");
    describe_gauge!(
        VALIDATOR_READY,
        "1 once the beacon node answered the startup reads that proving needs"
    );

    handle
}

/// Builds the Prometheus exporter with the buckets of every histogram.
pub fn prometheus_builder() -> PrometheusBuilder {
    let slot_buckets = from_fn::<_, 24, _>(|i| (i + 1) as f64 * 0.5);
    // The slot buckets resolve the delays within the slot, and the tail the delays past it.
    let late_buckets = [slot_buckets.as_slice(), LATE_TAIL_BUCKETS.as_slice()].concat();
    // 16 KiB to 64 MiB, doubling. The default buckets end at 10.0 and put every size in +Inf.
    let byte_buckets = from_fn::<_, 13, _>(|i| (16_384_u64 << i) as f64);
    PrometheusBuilder::new()
        .set_buckets(DEFAULT_BUCKETS)
        .unwrap()
        .set_buckets_for_metric(
            Matcher::Full(PROVE_DURATION_SECONDS.to_owned()),
            &slot_buckets,
        )
        .unwrap()
        // The witness fetch is the engine_newPayloadWithWitness round trip to the EL, which the
        // CL bounds by the slot, so give it the same 0.5s-step resolution up to 12.0 as the
        // prove buckets. The default buckets end at 10.0 and hide 10-12s fetches in +Inf.
        .set_buckets_for_metric(
            Matcher::Full(WITNESS_FETCH_DURATION_SECONDS.to_owned()),
            &slot_buckets,
        )
        .unwrap()
        .set_buckets_for_metric(
            Matcher::Full(QUEUE_WAIT_DURATION_SECONDS.to_owned()),
            QUEUE_WAIT_BUCKETS,
        )
        .unwrap()
        .set_buckets_for_metric(
            Matcher::Full(PAYLOAD_ARRIVAL_DELAY_SECONDS.to_owned()),
            &late_buckets,
        )
        .unwrap()
        // A proof later than the slot lands past the 12.0 bucket, which counts the proofs in time.
        .set_buckets_for_metric(
            Matcher::Full(PROOF_LATENCY_SECONDS.to_owned()),
            &late_buckets,
        )
        .unwrap()
        .set_buckets_for_metric(
            Matcher::Full(PROOF_SLOT_LATENCY_SECONDS.to_owned()),
            &late_buckets,
        )
        .unwrap()
        .set_buckets_for_metric(Matcher::Full(WITNESS_BYTES.to_owned()), &byte_buckets)
        .unwrap()
        .set_buckets_for_metric(Matcher::Full(PROVE_PROOF_BYTES.to_owned()), &byte_buckets)
        .unwrap()
}

/// Spawns a background task that calls `run_upkeep()` every 5 seconds.
pub fn spawn_upkeep(handle: PrometheusHandle) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            handle.run_upkeep();
        }
    });
}

/// Records an HTTP request completion with status and duration.
fn record_http_request(endpoint: &str, method: &str, status: u16, duration: Duration) {
    let endpoint = endpoint.to_owned();
    let method = method.to_owned();
    counter!(
        HTTP_REQUESTS_TOTAL,
        "endpoint" => endpoint.clone(),
        "method" => method.clone(),
        "status" => status.to_string()
    )
    .increment(1);
    histogram!(
        HTTP_REQUEST_DURATION_SECONDS,
        "endpoint" => endpoint,
        "method" => method
    )
    .record(duration.as_secs_f64());
}

/// Records the receipt at `received_at` of a payload whose slot starts at the Unix `timestamp`.
pub fn record_payload_arrival(timestamp: u64, received_at: SystemTime) {
    histogram!(PAYLOAD_ARRIVAL_DELAY_SECONDS).record(seconds_after(timestamp, received_at));
}

/// Sets the timestamp of the newest VALID payload, the start of its slot.
pub fn set_latest_payload_timestamp(timestamp: u64) {
    gauge!(LATEST_PAYLOAD_TIMESTAMP_SECONDS).set(timestamp as f64);
}

/// Records the status of the EL answer to an `engine_newPayload`, `error` without an answer.
pub fn record_new_payload(status: &'static str) {
    debug_assert!(NEW_PAYLOAD_STATUSES.contains(&status));
    counter!(NEW_PAYLOAD_TOTAL, "status" => status).increment(1);
}

/// Records a witness fetch result, where the fetch is the `engine_newPayloadWithWitness` round
/// trip to the EL. The duration and the size are recorded for a returned witness only.
pub fn record_witness_fetch(status: &'static str, duration: Duration, witness_size: usize) {
    debug_assert!(WITNESS_FETCH_STATUSES.contains(&status));
    counter!(WITNESS_FETCH_TOTAL, "status" => status).increment(1);
    if status == "success" {
        histogram!(WITNESS_FETCH_DURATION_SECONDS).record(duration.as_secs_f64());
        histogram!(WITNESS_BYTES).record(witness_size as f64);
    }
}

/// Records how long a proof request waited in the worker channel before a
/// worker dequeued it. This is the only place queue wait is observable. The
/// prove duration histogram starts after dequeue, so without this metric a
/// backlog is invisible.
pub fn record_queue_wait(proof_type: ProofType, duration: Duration) {
    histogram!(
        QUEUE_WAIT_DURATION_SECONDS,
        "proof_type" => proof_type.to_string(),
    )
    .record(duration.as_secs_f64());
}

/// Records a proof request that ends before its input reaches the worker.
pub fn record_proof_dispatch_failure(proof_type: ProofType, reason: &'static str) {
    debug_assert!(PROOF_DISPATCH_FAILURE_REASONS.contains(&reason));
    counter!(
        PROOF_DISPATCH_FAILURES_TOTAL,
        "proof_type" => proof_type.to_string(),
        "reason" => reason
    )
    .increment(1);
}

/// Adds `delta` to the number of proof inputs waiting in the channel of a worker.
pub fn add_queue_depth(proof_type: ProofType, delta: f64) {
    gauge!(QUEUE_DEPTH, "proof_type" => proof_type.to_string()).increment(delta);
}

/// Registers the counters of every known label value, the histograms without labels, and the
/// queue depth and the histograms of every configured proof type, at 0.
pub fn register_series(proof_types: impl IntoIterator<Item = ProofType>) {
    for name in [
        PAYLOAD_ARRIVAL_DELAY_SECONDS,
        WITNESS_FETCH_DURATION_SECONDS,
        WITNESS_BYTES,
    ] {
        let _ = histogram!(name);
    }
    for status in NEW_PAYLOAD_STATUSES {
        counter!(NEW_PAYLOAD_TOTAL, "status" => status).increment(0);
    }
    for status in WITNESS_FETCH_STATUSES {
        counter!(WITNESS_FETCH_TOTAL, "status" => status).increment(0);
    }
    for proof_type in proof_types {
        add_queue_depth(proof_type, 0.0);
        let proof_type = proof_type.to_string();
        counter!(PROOF_REUSED_TOTAL, "proof_type" => proof_type.clone()).increment(0);
        for status in PROVE_STATUSES {
            counter!(PROVE_TOTAL, "proof_type" => proof_type.clone(), "status" => status)
                .increment(0);
        }
        for status in SUBMISSION_STATUSES {
            counter!(SUBMISSION_TOTAL, "proof_type" => proof_type.clone(), "status" => status)
                .increment(0);
        }
        for reason in PROOF_DISPATCH_FAILURE_REASONS {
            counter!(PROOF_DISPATCH_FAILURES_TOTAL, "proof_type" => proof_type.clone(), "reason" => reason)
                .increment(0);
        }
        for name in [
            QUEUE_WAIT_DURATION_SECONDS,
            PROVE_DURATION_SECONDS,
            PROVE_PROOF_BYTES,
            SUBMISSION_DURATION_SECONDS,
            PROOF_LATENCY_SECONDS,
            PROOF_SLOT_LATENCY_SECONDS,
        ] {
            let _ = histogram!(name, "proof_type" => proof_type.clone());
        }
    }
}

/// Records a prove operation result.
pub fn record_prove(
    proof_type: ProofType,
    status: &'static str,
    duration: Duration,
    proof_size: usize,
) {
    debug_assert!(PROVE_STATUSES.contains(&status));
    counter!(
        PROVE_TOTAL,
        "proof_type" => proof_type.to_string(),
        "status" => status
    )
    .increment(1);
    if status == "success" {
        histogram!(
            PROVE_DURATION_SECONDS,
            "proof_type" => proof_type.to_string(),
        )
        .record(duration.as_secs_f64());
        histogram!(
            PROVE_PROOF_BYTES,
            "proof_type" => proof_type.to_string(),
        )
        .record(proof_size as f64);
    }
}

/// Records a cached proof submitted again for a payload imported again.
pub fn record_proof_reused(proof_type: ProofType) {
    counter!(PROOF_REUSED_TOTAL, "proof_type" => proof_type.to_string()).increment(1);
}

/// Records a proof submission result. The submission duration is recorded for an accepted
/// submission only.
pub fn record_submission(proof_type: ProofType, status: &'static str, duration: Duration) {
    debug_assert!(SUBMISSION_STATUSES.contains(&status));
    counter!(
        SUBMISSION_TOTAL,
        "proof_type" => proof_type.to_string(),
        "status" => status
    )
    .increment(1);
    if status == "success" {
        histogram!(
            SUBMISSION_DURATION_SECONDS,
            "proof_type" => proof_type.to_string(),
        )
        .record(duration.as_secs_f64());
    }
}

/// Records the latencies of an accepted proof since the payload receipt and since the start of
/// the slot at the Unix `timestamp`.
pub fn record_proof_latency(proof_type: ProofType, latency: Duration, timestamp: u64) {
    histogram!(
        PROOF_LATENCY_SECONDS,
        "proof_type" => proof_type.to_string(),
    )
    .record(latency.as_secs_f64());
    histogram!(
        PROOF_SLOT_LATENCY_SECONDS,
        "proof_type" => proof_type.to_string(),
    )
    .record(seconds_after(timestamp, SystemTime::now()));
}

/// Sets the number of loaded programs gauge.
pub fn set_programs_loaded(count: usize) {
    gauge!(PROGRAMS_LOADED).set(count as f64);
}

/// Sets the build info gauge with version label.
pub fn set_build_info(version: &str) {
    gauge!(BUILD_INFO, "version" => version.to_string()).set(1.0);
}

/// Sets whether the validator that proving needs is ready.
pub fn set_validator_ready(ready: bool) {
    gauge!(VALIDATOR_READY).set(f64::from(u8::from(ready)));
}

/// Returns the seconds from the Unix `timestamp` until `time`, zero for a later timestamp.
fn seconds_after(timestamp: u64, time: SystemTime) -> f64 {
    let time = time
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the Unix epoch");
    (time.as_secs_f64() - timestamp as f64).max(0.0)
}

/// The JSON-RPC method of an Engine API request, which the handler puts on every answer except a
/// JWT rejection of the EL.
#[derive(Debug, Clone)]
pub(crate) struct JsonRpcMethod(pub(crate) String);

/// Axum middleware that records HTTP request metrics.
///
/// Uses `MatchedPath` (the route template) rather than the raw URI to avoid
/// unbounded metric cardinality from path parameters. The requests and their duration are
/// recorded for a response with a [`JsonRpcMethod`] only, so a call that the EL rejected for the
/// JWT adds no series.
pub(crate) async fn http_metrics_middleware(request: Request, next: Next) -> Response {
    struct InFlightGuard {
        endpoint: String,
    }

    impl InFlightGuard {
        fn new(endpoint: String) -> Self {
            gauge!(HTTP_REQUESTS_IN_FLIGHT, "endpoint" => endpoint.clone()).increment(1.0);
            Self { endpoint }
        }
    }

    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            let endpoint = std::mem::take(&mut self.endpoint);
            gauge!(HTTP_REQUESTS_IN_FLIGHT, "endpoint" => endpoint).decrement(1.0);
        }
    }

    let method = request.method().to_string();
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched_path| matched_path.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());
    let _guard = InFlightGuard::new(path.clone());

    let start = Instant::now();
    let response = next.run(request).await;
    let elapsed = start.elapsed();

    if let Some(JsonRpcMethod(json_rpc_method)) = response.extensions().get::<JsonRpcMethod>() {
        record_http_request(
            json_rpc_method,
            &method,
            response.status().as_u16(),
            elapsed,
        );
    }

    response
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use axum::{
        Extension, Router,
        body::Body,
        http::{Request, StatusCode},
        middleware,
        routing::post,
    };
    use metrics::set_default_local_recorder;
    use tower::ServiceExt;
    use zkboost_types::ProofType;

    use crate::metrics::{
        JsonRpcMethod, http_metrics_middleware, prometheus_builder, record_payload_arrival,
        record_proof_latency,
    };

    /// Records with a recorder of the zkboost exporter and returns the exposition text.
    fn render(record: impl FnOnce()) -> String {
        let recorder = prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, record);
        handle.render()
    }

    /// Returns the count of the `name` bucket with the upper bound `le` and the label `labels`.
    fn bucket(text: &str, name: &str, labels: &str, le: f64) -> u64 {
        text.lines()
            .filter(|line| line.starts_with(&format!("{name}_bucket{{")) && line.contains(labels))
            .find_map(|line| {
                let bound = line.split("le=\"").nth(1)?.split('"').next()?;
                let bound = if bound == "+Inf" {
                    f64::INFINITY
                } else {
                    bound.parse().ok()?
                };
                (bound == le).then(|| line.rsplit(' ').next()?.parse().ok())?
            })
            .unwrap_or_else(|| panic!("no {name} bucket le={le} with {labels} in\n{text}"))
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn test_late_payload_arrival_stays_measurable() {
        let text = render(|| {
            record_payload_arrival(now() - 200, SystemTime::now());
            record_payload_arrival(now() + 60, SystemTime::now());
        });

        let name = "zkboost_payload_arrival_delay_seconds";
        assert_eq!(
            bucket(&text, name, "", 0.5),
            1,
            "a future timestamp counts as 0 s"
        );
        assert_eq!(bucket(&text, name, "", 12.0), 1);
        assert_eq!(bucket(&text, name, "", 180.0), 1);
        assert_eq!(bucket(&text, name, "", 300.0), 2);
    }

    #[test]
    fn test_late_proof_stays_measurable() {
        let text = render(|| {
            record_proof_latency(ProofType::RethOpenVM, Duration::from_secs(150), now() - 340);
        });

        let labels = r#"proof_type="reth-openvm""#;
        let latency = "zkboost_proof_latency_seconds";
        assert_eq!(bucket(&text, latency, labels, 12.0), 0);
        assert_eq!(bucket(&text, latency, labels, 120.0), 0);
        assert_eq!(bucket(&text, latency, labels, 180.0), 1);
        let slot_latency = "zkboost_proof_slot_latency_seconds";
        assert_eq!(bucket(&text, slot_latency, labels, 300.0), 0);
        assert_eq!(bucket(&text, slot_latency, labels, 600.0), 1);
        assert_eq!(bucket(&text, slot_latency, labels, f64::INFINITY), 1);
    }

    /// Only a response with a JSON-RPC method is recorded, so a call that the EL rejected and a
    /// custom HTTP method add no series.
    #[tokio::test]
    async fn test_only_calls_with_method_recorded() {
        let recorder = prometheus_builder().build_recorder();
        let _recorder_guard = set_default_local_recorder(&recorder);
        let router = Router::new()
            .route(
                "/",
                post(|method: String| async move {
                    let accepted = method == "engine_accepted";
                    let status = if accepted {
                        StatusCode::OK
                    } else {
                        StatusCode::UNAUTHORIZED
                    };
                    (
                        status,
                        accepted.then_some(Extension(JsonRpcMethod(method))),
                        (),
                    )
                }),
            )
            .layer(middleware::from_fn(http_metrics_middleware));

        for method in ["engine_accepted", "engine_rejected"] {
            let request = Request::post("/").body(Body::from(method)).unwrap();
            router.clone().oneshot(request).await.unwrap();
        }
        let request = Request::builder()
            .method("FOO")
            .uri("/")
            .body(Body::empty())
            .unwrap();
        router.oneshot(request).await.unwrap();

        let text = recorder.handle().render();
        let requests = text
            .lines()
            .filter(|line| line.starts_with("zkboost_http_requests_total{"))
            .collect::<Vec<_>>();
        assert_eq!(
            requests,
            [
                r#"zkboost_http_requests_total{endpoint="engine_accepted",method="POST",status="200"} 1"#
            ]
        );
    }
}

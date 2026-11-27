//! Every metric of zkboost, in the order of a payload through zkboost. One row of `METRICS` gives
//! the buckets and the help of a metric. Each component owns the handles of its series, which start
//! at 0 when the component is built, because `rate` and `increase` do not count the first event
//! of a series that starts after them.

use std::{
    array::from_fn,
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use metrics::{
    Counter, Gauge, Histogram, counter, describe_counter, describe_gauge, describe_histogram,
    gauge, histogram,
};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use strum::{EnumIter, IntoStaticStr, VariantNames};

use crate::{engine::payload::PayloadStatus, proof::proof_type::ProofType};

#[derive(Clone, Copy)]
enum Kind {
    Counter,
    Gauge,
    Histogram(Buckets),
}
use Kind::{Counter as C, Gauge as G, Histogram as H};

#[derive(Clone, Copy)]
enum Buckets {
    /// Durations of requests, 5 ms to 10 s.
    Request,
    /// Durations within the slot, in steps of 0.5 s.
    Slot,
    /// The slot buckets with a tail to 2 minutes, for a proof timeout longer than the slot.
    Prove,
    /// The slot buckets with a tail to 1 hour, so a delay of minutes keeps its size.
    Late,
    /// Waits from none to 15 minutes. The 1 ms bucket holds the wait of an idle prover.
    QueueWait,
    /// One bucket at the slot, for a metric that is read as a mean.
    Step,
    /// Sizes from 16 KiB to 64 MiB, doubling.
    Bytes,
}

impl Buckets {
    fn bounds(self) -> Vec<f64> {
        let slot = || (1..=24).map(|step| f64::from(step) * 0.5);
        match self {
            Self::Request => vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ],
            Self::Slot => slot().collect(),
            Self::Prove => slot().chain([24.0, 36.0, 48.0, 60.0, 120.0]).collect(),
            Self::Late => slot()
                .chain([
                    24.0, 36.0, 60.0, 120.0, 180.0, 240.0, 300.0, 360.0, 420.0, 480.0, 540.0,
                    600.0, 1800.0, 3600.0,
                ])
                .collect(),
            Self::QueueWait => vec![
                0.001, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0,
                900.0,
            ],
            Self::Step => vec![12.0],
            Self::Bytes => (0..13)
                .map(|shift| f64::from(16_384_u32 << shift))
                .collect(),
        }
    }
}

#[rustfmt::skip]
const METRICS: &[(&str, Kind, &str)] = &[
    // Engine API
    ("zkboost_http_requests_total", C, "engine api requests by json-rpc method"),
    ("zkboost_http_request_duration_seconds", H(Buckets::Request), "engine api request duration by json-rpc method"),
    ("zkboost_http_requests_in_flight", G, "http requests in flight"),
    // Payload and witness
    ("zkboost_payload_arrival_delay_seconds", H(Buckets::Late), "time from the start of the slot of a payload to its engine_newPayload receipt"),
    ("zkboost_latest_payload_timestamp_seconds", G, "timestamp of the newest VALID payload, the start of its slot"),
    ("zkboost_new_payload_total", C, "engine_newPayload requests by the status of the el response"),
    ("zkboost_witness_fetch_total", C, "engine_newPayloadWithWitness round trips by result"),
    ("zkboost_witness_fetch_duration_seconds", H(Buckets::Slot), "engine_newPayloadWithWitness round trip time of a returned witness"),
    ("zkboost_witness_bytes", H(Buckets::Bytes), "witness size"),
    // Prover
    ("zkboost_proof_dispatch_failures_total", C, "proof requests that end before their input reaches the prover"),
    ("zkboost_queue_depth", G, "proof inputs waiting for the prover, without the input being proven"),
    ("zkboost_queue_wait_duration_seconds", H(Buckets::QueueWait), "time a proof request waits for the prover"),
    ("zkboost_prove_total", C, "prove operations by result"),
    ("zkboost_prove_duration_seconds", H(Buckets::Prove), "proof generation duration"),
    ("zkboost_prove_proof_bytes", H(Buckets::Bytes), "proof size"),
    // Submission
    ("zkboost_proof_reused_total", C, "cached proofs submitted again for a payload imported again"),
    ("zkboost_submission_total", C, "proof submissions to the beacon node by result"),
    ("zkboost_submission_duration_seconds", H(Buckets::Request), "duration of an accepted submission, with the beacon block root lookup and the retries"),
    ("zkboost_proof_latency_seconds", H(Buckets::Late), "time from the payload receipt to the accepted proof"),
    ("zkboost_proof_slot_latency_seconds", H(Buckets::Late), "time from the start of the slot of the payload to the accepted proof"),
    ("zkboost_proof_step_seconds", H(Buckets::Step), "time of each step of an accepted proof"),
    // Process
    ("zkboost_programs_loaded", G, "zkvms ready to prove"),
    ("zkboost_validator_ready", G, "1 once the beacon node responded to the startup reads that proving needs"),
    ("zkboost_build_info", G, "build info"),
];

/// Returns the Prometheus exporter with the buckets of every histogram.
pub fn builder() -> PrometheusBuilder {
    let builder = PrometheusBuilder::new()
        .set_buckets(&Buckets::Request.bounds())
        .expect("buckets are not empty");
    METRICS
        .iter()
        .fold(builder, |builder, &(name, kind, _)| match kind {
            H(buckets) => builder
                .set_buckets_for_metric(Matcher::Full(name.to_owned()), &buckets.bounds())
                .expect("buckets are not empty"),
            C | G => builder,
        })
}

/// Installs the global Prometheus recorder and returns the handle that renders `/metrics`.
pub fn install() -> PrometheusHandle {
    builder()
        .install_recorder()
        .expect("the recorder is installed once")
}

/// Gives every metric its help text, and reports the build.
pub(crate) fn describe() {
    for &(name, kind, help) in METRICS {
        match kind {
            C => describe_counter!(name, help),
            G => describe_gauge!(name, help),
            H(_) => describe_histogram!(name, help),
        }
    }
    gauge!("zkboost_build_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);
}

pub(crate) fn validator_ready() -> Gauge {
    gauge!("zkboost_validator_ready")
}

type Labels<'a> = &'a [(&'static str, &'static str)];

/// Returns the counter of each value of the label `key`, in the order of `L::VARIANTS`.
fn counters<L: VariantNames, const N: usize>(
    name: &'static str,
    labels: Labels<'_>,
    key: &'static str,
) -> [Counter; N] {
    assert_eq!(L::VARIANTS.len(), N);
    from_fn(|index| counter!(name, &[labels, &[(key, L::VARIANTS[index])]].concat()))
}

/// The result of an `engine_newPayloadWithWitness` round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, IntoStaticStr, VariantNames)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum WitnessFetchStatus {
    /// A `VALID` payload with its witness.
    Success,
    /// A `VALID` payload without a witness, or an EL that does not serve the method.
    Missing,
    NotValid,
    Error,
}

/// The reason a proof request ends before its input reaches the prover.
#[derive(Debug, Clone, Copy, EnumIter, IntoStaticStr, VariantNames)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum DispatchFailure {
    InputError,
    MissingWitness,
    UnsupportedFork,
    QueueFull,
}

#[derive(Debug, Clone, Copy, EnumIter, IntoStaticStr, VariantNames)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum ProveStatus {
    Success,
    Error,
    Timeout,
}

#[derive(Debug, Clone, Copy, EnumIter, IntoStaticStr, VariantNames)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum SubmissionStatus {
    Success,
    Error,
}

/// The time of each step of an accepted proof. The steps add up to its latency from the start
/// of the slot.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProofSteps {
    /// The time from the start of the slot until the receipt of the payload.
    pub(crate) payload_arrival: Duration,
    pub(crate) witness_fetch: Duration,
    pub(crate) queue_wait: Duration,
    pub(crate) prove: Duration,
    pub(crate) submission: Duration,
}

impl ProofSteps {
    const NAMES: [&str; 5] = [
        "payload_arrival",
        "witness_fetch",
        "queue_wait",
        "prove",
        "submission",
    ];

    fn durations(&self) -> [Duration; 5] {
        [
            self.payload_arrival,
            self.witness_fetch,
            self.queue_wait,
            self.prove,
            self.submission,
        ]
    }
}

pub(crate) struct ProxyMetrics {
    in_flight: Gauge,
    witness_fetch: [Counter; 4],
    witness_fetch_duration: Histogram,
    witness_bytes: Histogram,
}

impl ProxyMetrics {
    pub(crate) fn new() -> Self {
        Self {
            in_flight: gauge!("zkboost_http_requests_in_flight", "endpoint" => "/"),
            witness_fetch: counters::<WitnessFetchStatus, 4>(
                "zkboost_witness_fetch_total",
                &[],
                "status",
            ),
            witness_fetch_duration: histogram!("zkboost_witness_fetch_duration_seconds"),
            witness_bytes: histogram!("zkboost_witness_bytes"),
        }
    }

    /// Counts a request in flight until the guard drops.
    pub(crate) fn in_flight(&self) -> impl Drop + '_ {
        struct InFlight<'a>(&'a Gauge);
        impl Drop for InFlight<'_> {
            fn drop(&mut self) {
                self.0.decrement(1.0);
            }
        }
        self.in_flight.increment(1.0);
        InFlight(&self.in_flight)
    }

    /// Records an Engine API request. The method is the one label without known values, so
    /// its series start with their first request.
    pub(crate) fn request(&self, method: &str, status: StatusCode, duration: Duration) {
        let (endpoint, status) = (method.to_owned(), status.as_str().to_owned());
        counter!("zkboost_http_requests_total", "endpoint" => endpoint.clone(), "method" => "POST", "status" => status)
            .increment(1);
        histogram!("zkboost_http_request_duration_seconds", "endpoint" => endpoint, "method" => "POST")
            .record(duration);
    }

    /// Records a witness fetch. The duration and the size are those of a returned witness.
    pub(crate) fn witness_fetched(
        &self,
        status: WitnessFetchStatus,
        duration: Duration,
        size: usize,
    ) {
        self.witness_fetch[status as usize].increment(1);
        if status == WitnessFetchStatus::Success {
            self.witness_fetch_duration.record(duration);
            self.witness_bytes.record(size as f64);
        }
    }
}

pub(crate) struct DispatcherMetrics {
    new_payload: [Counter; 5],
    payload_arrival: Histogram,
    latest_payload_timestamp: Gauge,
}

impl DispatcherMetrics {
    pub(crate) fn new() -> Self {
        // Without a prover, no prover registers this gauge.
        let _ = gauge!("zkboost_programs_loaded");
        Self {
            new_payload: counters::<PayloadStatus, 5>("zkboost_new_payload_total", &[], "status"),
            payload_arrival: histogram!("zkboost_payload_arrival_delay_seconds"),
            latest_payload_timestamp: gauge!("zkboost_latest_payload_timestamp_seconds"),
        }
    }

    pub(crate) fn new_payload(&self, status: PayloadStatus) {
        self.new_payload[status as usize].increment(1);
    }

    /// Records the time from the start of the slot of a payload until its receipt.
    pub(crate) fn payload_arrived(&self, arrival: Duration) {
        self.payload_arrival.record(arrival);
    }

    pub(crate) fn set_latest_payload_timestamp(&self, timestamp: u64) {
        self.latest_payload_timestamp.set(timestamp as f64);
    }
}

/// The metrics of one prover, with its proof type as a label.
pub(crate) struct ProverMetrics {
    programs_loaded: Gauge,
    dispatch_failures: [Counter; 4],
    queue_depth: Gauge,
    queue_wait: Histogram,
    prove: [Counter; 3],
    prove_duration: Histogram,
    proof_bytes: Histogram,
    reused: Counter,
    submission: [Counter; 2],
    submission_duration: Histogram,
    proof_latency: Histogram,
    proof_slot_latency: Histogram,
    proof_steps: [Histogram; 5],
}

impl ProverMetrics {
    pub(crate) fn new(proof_type: ProofType) -> Self {
        let proof_type = proof_type.as_str();
        let labels: Labels<'_> = &[("proof_type", proof_type)];
        Self {
            programs_loaded: gauge!("zkboost_programs_loaded"),
            dispatch_failures: counters::<DispatchFailure, 4>(
                "zkboost_proof_dispatch_failures_total",
                labels,
                "reason",
            ),
            queue_depth: gauge!("zkboost_queue_depth", labels),
            queue_wait: histogram!("zkboost_queue_wait_duration_seconds", labels),
            prove: counters::<ProveStatus, 3>("zkboost_prove_total", labels, "status"),
            prove_duration: histogram!("zkboost_prove_duration_seconds", labels),
            proof_bytes: histogram!("zkboost_prove_proof_bytes", labels),
            reused: counter!("zkboost_proof_reused_total", labels),
            submission: counters::<SubmissionStatus, 2>("zkboost_submission_total", labels, "status"),
            submission_duration: histogram!("zkboost_submission_duration_seconds", labels),
            proof_latency: histogram!("zkboost_proof_latency_seconds", labels),
            proof_slot_latency: histogram!("zkboost_proof_slot_latency_seconds", labels),
            proof_steps: ProofSteps::NAMES.map(|step| {
                histogram!("zkboost_proof_step_seconds", "proof_type" => proof_type, "step" => step)
            }),
        }
    }

    pub(crate) fn connected(&self) {
        self.programs_loaded.increment(1.0);
    }

    pub(crate) fn dispatch_failed(&self, reason: DispatchFailure) {
        self.dispatch_failures[reason as usize].increment(1);
    }

    /// Sets the number of proofs that wait for the prover.
    pub(crate) fn set_queue_depth(&self, proofs: usize) {
        self.queue_depth.set(proofs as f64);
    }

    pub(crate) fn queue_waited(&self, duration: Duration) {
        self.queue_wait.record(duration);
    }

    /// Records a proof generation. The duration and the size are those of a generated proof.
    pub(crate) fn proved(&self, status: ProveStatus, duration: Duration, proof_size: usize) {
        self.prove[status as usize].increment(1);
        if let ProveStatus::Success = status {
            self.prove_duration.record(duration);
            self.proof_bytes.record(proof_size as f64);
        }
    }

    pub(crate) fn reused(&self) {
        self.reused.increment(1);
    }

    /// Records a submission. The duration is that of an accepted submission.
    pub(crate) fn submitted(&self, status: SubmissionStatus, duration: Duration) {
        self.submission[status as usize].increment(1);
        if let SubmissionStatus::Success = status {
            self.submission_duration.record(duration);
        }
    }

    /// Records the latencies of an accepted proof of a payload received at `received`.
    pub(crate) fn accepted(&self, received: Instant, steps: &ProofSteps) {
        let latency = received.elapsed();
        self.proof_latency.record(latency);
        self.proof_slot_latency
            .record(steps.payload_arrival + latency);
        for (histogram, duration) in self.proof_steps.iter().zip(steps.durations()) {
            histogram.record(duration);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        time::{Duration, Instant},
    };

    use axum::http::StatusCode;
    use strum::IntoEnumIterator;

    use crate::{
        engine::payload::PayloadStatus,
        observability::metrics::{
            DispatchFailure, DispatcherMetrics, METRICS, ProofSteps, ProveStatus, ProverMetrics,
            ProxyMetrics, SubmissionStatus, WitnessFetchStatus, builder, describe, validator_ready,
        },
        proof::proof_type::ProofType,
    };

    /// Returns the series of the exposition text, each a name with its labels.
    fn series(text: &str) -> BTreeSet<&str> {
        text.lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .map(|line| line.rsplit_once(' ').unwrap().0)
            .collect()
    }

    /// Every event records into a series that exists since its component was built, except
    /// the request series, whose method label has no known values. Every metric has a row in
    /// `METRICS`, and every row has a series.
    #[test]
    fn test_every_event_records_into_a_series_of_its_component() {
        let recorder = builder().build_recorder();
        let handle = recorder.handle();
        let (proxy, dispatcher, prover) = metrics::with_local_recorder(&recorder, || {
            describe();
            let prover = ProverMetrics::new(ProofType::RethZisk);
            (ProxyMetrics::new(), DispatcherMetrics::new(), prover)
        });
        let validator_ready = metrics::with_local_recorder(&recorder, validator_ready);
        let built = handle.render();

        let duration = Duration::from_secs(1);
        let steps = ProofSteps {
            payload_arrival: duration,
            witness_fetch: duration,
            queue_wait: duration,
            prove: duration,
            submission: duration,
        };
        drop(proxy.in_flight());
        WitnessFetchStatus::iter().for_each(|status| proxy.witness_fetched(status, duration, 1));
        PayloadStatus::iter().for_each(|status| dispatcher.new_payload(status));
        dispatcher.payload_arrived(duration);
        dispatcher.set_latest_payload_timestamp(1);
        prover.connected();
        DispatchFailure::iter().for_each(|reason| prover.dispatch_failed(reason));
        prover.set_queue_depth(0);
        prover.queue_waited(duration);
        ProveStatus::iter().for_each(|status| prover.proved(status, duration, 1));
        prover.reused();
        SubmissionStatus::iter().for_each(|status| prover.submitted(status, duration));
        prover.accepted(Instant::now(), &steps);
        validator_ready.set(1.0);
        assert_eq!(series(&handle.render()), series(&built));

        metrics::with_local_recorder(&recorder, || {
            proxy.request("engine_newPayloadV5", StatusCode::OK, duration)
        });
        let recorded = handle.render();
        let names = |suffixes: &[&str]| {
            let metric_name = |line: &&str| {
                let name = line.split('{').next().unwrap();
                let suffix = suffixes.iter().find(|suffix| name.ends_with(**suffix));
                suffix
                    .map_or(name, |suffix| &name[..name.len() - suffix.len()])
                    .to_owned()
            };
            BTreeSet::from_iter(series(&recorded).iter().map(metric_name))
        };
        let rows = BTreeSet::from_iter(METRICS.iter().map(|row| row.0.to_owned()));
        assert_eq!(names(&["_bucket", "_sum", "_count"]), rows);
    }

    /// A late payload, a late proof, and a slow prove land in the tail buckets instead of +Inf.
    #[test]
    fn test_tail_buckets() {
        let recorder = builder().build_recorder();
        let (dispatcher, prover) = metrics::with_local_recorder(&recorder, || {
            (
                DispatcherMetrics::new(),
                ProverMetrics::new(ProofType::RethOpenvm),
            )
        });
        dispatcher.payload_arrived(Duration::from_secs(200));
        let [
            payload_arrival,
            witness_fetch,
            queue_wait,
            prove,
            submission,
        ] = [190, 0, 100, 10, 0].map(Duration::from_secs);
        let steps = ProofSteps {
            payload_arrival,
            witness_fetch,
            queue_wait,
            prove,
            submission,
        };
        prover.accepted(Instant::now() - Duration::from_secs(150), &steps);
        prover.proved(ProveStatus::Success, Duration::from_secs(100), 1);

        let text = recorder.handle().render();
        let openvm = r#"proof_type="reth-openvm""#;
        for (name, labels, le, count) in [
            ("zkboost_payload_arrival_delay_seconds", "", "180", 0),
            ("zkboost_payload_arrival_delay_seconds", "", "240", 1),
            ("zkboost_proof_latency_seconds", openvm, "120", 0),
            ("zkboost_proof_latency_seconds", openvm, "180", 1),
            // The proof is accepted 190 + 150 = 340 s after the slot start.
            ("zkboost_proof_slot_latency_seconds", openvm, "300", 0),
            ("zkboost_proof_slot_latency_seconds", openvm, "360", 1),
            (
                "zkboost_proof_step_seconds",
                r#"step="payload_arrival""#,
                "12",
                0,
            ),
            (
                "zkboost_proof_step_seconds",
                r#"step="payload_arrival""#,
                "+Inf",
                1,
            ),
            ("zkboost_prove_duration_seconds", openvm, "60", 0),
            ("zkboost_prove_duration_seconds", openvm, "120", 1),
        ] {
            let bucket = text
                .lines()
                .find(|line| {
                    line.starts_with(&format!("{name}_bucket{{"))
                        && line.contains(labels)
                        && line.contains(&format!(r#"le="{le}""#))
                })
                .unwrap_or_else(|| panic!("no {name} bucket le={le}"));
            assert!(bucket.ends_with(&format!(" {count}")), "{bucket}");
        }
    }
}

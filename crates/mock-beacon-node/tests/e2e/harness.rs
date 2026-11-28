//! The vocabulary of the cases, and the harness that runs a case. The harness puts zkboost and
//! the mock beacon node between the fakes.

use std::{
    convert::Infallible,
    net::Ipv4Addr,
    sync::{Arc, LazyLock, atomic::Ordering},
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use metrics::LocalRecorderGuard;
use mock_beacon_node::MockBeaconNode;
use opentelemetry::{global, trace::TracerProvider};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{InMemorySpanExporter, SdkTracerProvider, SpanData},
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, time::sleep};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;
use zkboost_server::Server;

use crate::fakes::{BLOCK_NUMBER, DIRECT, FIXTURE, Fakes, GAS_USED, PARAMS, TIMESTAMP, WITNESS};

pub(crate) const NEW_PAYLOAD: &str = "engine_newPayloadV5";

pub(crate) const NEW_PAYLOAD_WITH_WITNESS: &str = "engine_newPayloadWithWitnessV5";

/// The span of the CL in the `traceparent` of every request.
const CL_SPAN_ID: &str = "b7ad6b7169203331";

/// The size of a mock proof.
const PROOF_SIZE: u64 = 256 << 10;

const SLOW_UPLOAD: Duration = Duration::from_millis(200);

pub(crate) struct Case {
    pub(crate) scenario: Scenario,
    pub(crate) steps: &'static [Step],
    /// The methods of the calls that the EL gets from zkboost.
    pub(crate) el_calls: &'static [&'static str],
}

/// The behavior of the fake EL, of the fake CL, and of the mock provers of zkboost.
#[derive(Clone, Copy)]
pub(crate) struct Scenario {
    pub(crate) payload_status: &'static str,
    pub(crate) witness_unsupported: bool,
    /// The witness comes with every response, not with the first one only as geth sends it.
    pub(crate) witness_every_call: bool,
    pub(crate) el_unreachable: bool,
    pub(crate) cl_unreachable: bool,
    /// The first spec that the CL serves has a value that makes Lighthouse panic.
    pub(crate) spec_invalid_once: bool,
    /// Two beacon blocks of the slot carry the payload, which is an equivocation of the proposer.
    pub(crate) ambiguous_block: bool,
    /// The bid of a beacon block at the next slot names the payload. Such a bid costs its
    /// proposer nothing, and its block can never carry the payload.
    pub(crate) other_slot_bid: bool,
    /// The CL announces the beacon block with an event and lists no child of the parent.
    pub(crate) block_from_event: bool,
    /// The CL responds with 503 to the first read of the headers.
    pub(crate) headers_error_once: bool,
    /// The CL responds with 503 to the first subscription to the events.
    pub(crate) events_error_once: bool,
    /// The proof types of the mock provers.
    pub(crate) provers: &'static [&'static str],
    /// One more prover is an ere-server that does not respond.
    pub(crate) ere_unreachable: bool,
    pub(crate) proving_ms: u64,
    pub(crate) proof_timeout_secs: u64,
    pub(crate) proof_failure: bool,
}

impl Scenario {
    /// An EL and a CL that work, and one prover that proves in one second.
    pub(crate) const DEFAULT: Self = Self {
        payload_status: "VALID",
        witness_unsupported: false,
        witness_every_call: false,
        el_unreachable: false,
        cl_unreachable: false,
        spec_invalid_once: false,
        ambiguous_block: false,
        other_slot_bid: false,
        block_from_event: false,
        headers_error_once: false,
        events_error_once: false,
        provers: &["reth-openvm"],
        ere_unreachable: false,
        proving_ms: 1000,
        proof_timeout_secs: 12,
        proof_failure: false,
    };
}

#[derive(Clone, Copy)]
pub(crate) enum Step {
    /// The CL sends the fixture payload through the mock beacon node, and receives the response
    /// of the EL to `engine_newPayloadV5`.
    NewPayload,
    /// The CL sends the fixture payload two times at once.
    NewPayloadTwice,
    /// A caller calls a method of zkboost, and receives the response of the EL, byte for byte,
    /// or 502 for an EL that is unreachable. The call is counted under its method and status.
    Call(&'static str),
    /// A caller takes two times `SLOW_UPLOAD` to upload a call to zkboost. The duration of the
    /// request counts the upload.
    SlowCall(&'static str),
    /// A call that the EL rejects. It is not counted.
    CallWithoutJwt(&'static str),
    /// A caller posts a body that is no JSON-RPC request to zkboost. It is not counted.
    Post(&'static str),
    /// A caller calls a method with a param of 3 MiB, which is above the default body limit of
    /// axum.
    CallLarge(&'static str),
    /// A caller gets a path of zkboost and reads the status.
    Get(&'static str, u16),
    /// Waits until zkboost counts exactly this outcome.
    Wait(Outcome),
}

/// What zkboost counts in `/metrics`. Each count of a proof holds for every prover.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Outcome {
    pub(crate) valid_payloads: u64,
    pub(crate) invalid_payloads: u64,
    /// Responses to the witness method with a witness, without one, and for a payload that is not
    /// valid.
    pub(crate) witnesses: u64,
    pub(crate) missing_witnesses: u64,
    pub(crate) not_valid_witnesses: u64,
    pub(crate) dispatch_failures: u64,
    /// Proof generations that got the turn of the prover.
    pub(crate) started: u64,
    pub(crate) proved: u64,
    pub(crate) prove_errors: u64,
    pub(crate) prove_timeouts: u64,
    /// Proofs from the cache.
    pub(crate) reused: u64,
    pub(crate) submitted: u64,
    pub(crate) submission_errors: u64,
    /// Proofs that the beacon node accepted after their generation.
    pub(crate) accepted: u64,
}

impl Outcome {
    pub(crate) const NONE: Self = Self {
        valid_payloads: 0,
        invalid_payloads: 0,
        witnesses: 0,
        missing_witnesses: 0,
        not_valid_witnesses: 0,
        dispatch_failures: 0,
        started: 0,
        proved: 0,
        prove_errors: 0,
        prove_timeouts: 0,
        reused: 0,
        submitted: 0,
        submission_errors: 0,
        accepted: 0,
    };

    /// Reads the outcome from the text of `/metrics`, with the counts of one prover.
    fn read(metrics: &str, prover: Option<&str>) -> Self {
        // Sums the series of a metric whose labels start with `labels`.
        let sum = |name: &str, labels: String| {
            let prefix = format!("zkboost_{name}{{{labels}");
            let series = metrics
                .lines()
                .filter_map(|line| line.strip_prefix(&prefix));
            let values = series.map(|series| series.rsplit_once(' ').unwrap().1.parse().unwrap());
            let values = Vec::<f64>::from_iter(values);
            assert!(!values.is_empty(), "no {prefix} in /metrics");
            values.iter().sum::<f64>() as u64
        };
        let by_status = |name, status| sum(name, format!(r#"status="{status}""#));
        let of_prover = |name, labels| {
            prover.map_or(0, |prover| {
                sum(name, format!(r#"proof_type="{prover}"{labels}"#))
            })
        };
        Self {
            valid_payloads: by_status("new_payload_total", "VALID"),
            invalid_payloads: by_status("new_payload_total", "INVALID"),
            witnesses: by_status("witness_fetch_total", "success"),
            missing_witnesses: by_status("witness_fetch_total", "missing"),
            not_valid_witnesses: by_status("witness_fetch_total", "not_valid"),
            dispatch_failures: of_prover("proof_dispatch_failures_total", ""),
            started: of_prover("queue_wait_duration_seconds_count", ""),
            proved: of_prover("prove_total", r#",status="success""#),
            prove_errors: of_prover("prove_total", r#",status="error""#),
            prove_timeouts: of_prover("prove_total", r#",status="timeout""#),
            reused: of_prover("proof_reused_total", ""),
            submitted: of_prover("submission_total", r#",status="success""#),
            submission_errors: of_prover("submission_total", r#",status="error""#),
            accepted: of_prover("proof_slot_latency_seconds_count", ""),
        }
    }
}

/// Returns the value of a series in the text of `/metrics`.
fn series_value(metrics: &str, series: &str) -> f64 {
    let mut lines = metrics.lines();
    let value = lines.find_map(|line| line.strip_prefix(series)?.strip_prefix(' '));
    let value = value.unwrap_or_else(|| panic!("no {series} in /metrics"));
    value.parse().unwrap()
}

/// The spans of every test, exported by the one subscriber of the process. A subscriber of one
/// test thread does not get the spans whose call sites another thread registers first.
static SPANS: LazyLock<(InMemorySpanExporter, SdkTracerProvider)> = LazyLock::new(|| {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    global::set_text_map_propagator(TraceContextPropagator::new());
    let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("zkboost"));
    tracing_subscriber::registry().with(layer).init();
    (exporter, provider)
});

impl Case {
    pub(crate) async fn run(self) {
        let harness = Harness::new(self.scenario).await;
        for &step in self.steps {
            harness.step(step).await;
        }
        assert_eq!(*harness.fakes.el_calls.lock().unwrap(), self.el_calls);
        harness.check_invariants().await;
    }
}

async fn listen() -> (TcpListener, Url) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (listener, url.parse().unwrap())
}

struct Harness {
    scenario: Scenario,
    fakes: Arc<Fakes>,
    /// The Engine API of the mock beacon node, of zkboost, and of the fake EL.
    mock_engine: Url,
    zkboost: Url,
    el: Url,
    /// The trace of the CL, which every request carries.
    trace_id: String,
    http: reqwest::Client,
    shutdown: CancellationToken,
    /// Makes the recorder that `/metrics` renders the default of the test thread, where every
    /// task of the current-thread runtime records.
    _recorder: LocalRecorderGuard<'static>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl Harness {
    async fn new(scenario: Scenario) -> Self {
        LazyLock::force(&SPANS);
        let recorder = Box::leak(Box::new(
            zkboost_server::observability::metrics::builder().build_recorder(),
        ));
        let _recorder = metrics::set_default_local_recorder(recorder);

        let fakes = Fakes::new(scenario);
        let (el_listener, el) = listen().await;
        let (cl_listener, cl) = listen().await;
        let (mock_beacon_listener, mock_beacon) = listen().await;
        let (mock_engine_listener, mock_engine) = listen().await;
        tokio::spawn(axum::serve(el_listener, fakes.clone().el()).into_future());
        tokio::spawn(axum::serve(cl_listener, fakes.clone().cl()).into_future());

        // Nothing listens on port 1.
        let unreachable = "http://127.0.0.1:1";
        let el_engine_endpoint = [el.as_str(), unreachable][usize::from(scenario.el_unreachable)];
        let cl_beacon_endpoint =
            [mock_beacon.as_str(), unreachable][usize::from(scenario.cl_unreachable)];
        let Scenario {
            proving_ms,
            proof_timeout_secs,
            proof_failure,
            ..
        } = scenario;
        let zkvm = scenario.provers.iter().map(|proof_type| {
            format!(
                r#"{{ kind = "mock", proof_type = "{proof_type}", proof_timeout_secs = {proof_timeout_secs}, mock_proving_time = {{ kind = "constant", ms = {proving_ms} }}, mock_failure = {proof_failure} }}"#
            )
        });
        let ere =
            format!(r#"{{ kind = "ere", proof_type = "reth-zisk", endpoint = "{unreachable}" }}"#);
        let zkvm = zkvm.chain(scenario.ere_unreachable.then_some(ere));
        let config = format!(
            r#"
            port = 0
            el_engine_endpoint = "{el_engine_endpoint}"
            cl_beacon_endpoint = "{cl_beacon_endpoint}"
            validator_keystore_path = "{FIXTURE}/voting-keystore.json"
            validator_keystore_password_path = "{FIXTURE}/voting-keystore-password"
            dashboard = {{ enabled = true }}
            zkvm = [{}]
            "#,
            Vec::from_iter(zkvm).join(", ")
        );
        let server = Server::bind(config.parse().unwrap(), recorder.handle())
            .await
            .unwrap();
        let port = server.local_addr().port();
        let zkboost: Url = format!("http://127.0.0.1:{port}").parse().unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(server.run(shutdown.clone()));
        let node = MockBeaconNode::new(cl, zkboost.clone(), r#"{"execution_proofs":[]}"#).unwrap();
        tokio::spawn(Arc::new(node).serve(mock_beacon_listener, mock_engine_listener));

        let harness = Self {
            scenario,
            fakes,
            mock_engine,
            zkboost,
            el,
            trace_id: format!("{port:032x}"),
            http: reqwest::Client::new(),
            shutdown,
            _recorder,
        };
        // zkboost proves once the validator and every prover are ready.
        let ready = [1.0, scenario.provers.len() as f64];
        let deadline = Instant::now() + Duration::from_secs(30);
        while !scenario.cl_unreachable && !scenario.provers.is_empty() {
            let metrics = harness.metrics().await;
            let gauges = ["zkboost_validator_ready", "zkboost_programs_loaded"];
            if gauges.map(|gauge| series_value(&metrics, gauge)) == ready {
                break;
            }
            assert!(Instant::now() < deadline, "zkboost is not ready");
            sleep(Duration::from_millis(50)).await;
        }
        harness
    }

    /// Posts a body to an Engine API and returns the status and the response. zkboost forwards only
    /// the JWT, so the `DIRECT` mark reaches the EL only from the test itself.
    async fn post(&self, url: &Url, body: &str, jwt: bool) -> (StatusCode, String) {
        let traceparent = format!("00-{}-{CL_SPAN_ID}-01", self.trace_id);
        let mut request = (self.http.post(url.clone()).body(body.to_owned()))
            .header("traceparent", traceparent)
            .header(DIRECT, "1");
        if jwt {
            request = request.bearer_auth("jwt");
        }
        let response = request.send().await.unwrap();
        (response.status(), response.text().await.unwrap())
    }

    async fn metrics(&self) -> String {
        let url = self.zkboost.join("metrics").unwrap();
        let response = self.http.get(url).send().await.unwrap();
        response.text().await.unwrap()
    }

    /// Returns the outcome that zkboost counts, for each prover, or one without a prover.
    async fn outcomes(&self) -> Vec<Outcome> {
        let metrics = self.metrics().await;
        let provers = self.scenario.provers;
        match provers.is_empty() {
            true => vec![Outcome::read(&metrics, None)],
            false => Vec::from_iter(
                provers
                    .iter()
                    .map(|prover| Outcome::read(&metrics, Some(prover))),
            ),
        }
    }

    async fn step(&self, step: Step) {
        match step {
            Step::NewPayload => self.new_payload().await,
            Step::NewPayloadTwice => {
                tokio::join!(self.new_payload(), self.new_payload());
            }
            Step::Call(method) => {
                self.assert_forwarded(&call(method), true, Some(method))
                    .await
            }
            Step::SlowCall(method) => {
                let body = call(method).into_bytes();
                let halves = body.chunks(body.len() / 2 + 1).map(<[u8]>::to_vec);
                let upload = tokio_stream::iter(Vec::from_iter(halves)).then(|half| async {
                    sleep(SLOW_UPLOAD).await;
                    Ok::<_, Infallible>(half)
                });
                let request = (self.http.post(self.zkboost.clone()).bearer_auth("jwt"))
                    .body(reqwest::Body::wrap_stream(upload));
                assert_eq!(request.send().await.unwrap().status(), 200);
                let duration_series = format!(
                    r#"zkboost_http_request_duration_seconds_sum{{endpoint="{method}",method="POST"}}"#
                );
                let duration = series_value(&self.metrics().await, &duration_series);
                assert!(duration >= SLOW_UPLOAD.as_secs_f64(), "{duration}");
            }
            Step::CallWithoutJwt(method) => self.assert_forwarded(&call(method), false, None).await,
            Step::Post(body) => self.assert_forwarded(body, true, None).await,
            Step::CallLarge(method) => {
                let params = ["x".repeat(3 << 20)];
                let call = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
                self.assert_forwarded(&call.to_string(), true, Some(method))
                    .await
            }
            Step::Get(path, status) => {
                let url = self.zkboost.join(path).unwrap();
                assert_eq!(self.http.get(url).send().await.unwrap().status(), status);
            }
            Step::Wait(expected) => {
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    let outcomes = self.outcomes().await;
                    if outcomes.iter().all(|outcome| *outcome == expected) {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "expected {expected:#?}, found {outcomes:#?}"
                    );
                    sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    async fn new_payload(&self) {
        let request =
            json!({ "jsonrpc": "2.0", "id": 7, "method": NEW_PAYLOAD, "params": *PARAMS });
        let (_, expected) = self.post(&self.el, &request.to_string(), true).await;
        let (status, response) = self
            .post(&self.mock_engine, &request.to_string(), true)
            .await;
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            serde_json::from_str::<Value>(&expected).unwrap()
        );
    }

    /// Sends a body to zkboost and to the EL, and compares the responses. The request is counted
    /// one time under `counted`, and not at all without it.
    async fn assert_forwarded(&self, body: &str, jwt: bool, counted: Option<&str>) {
        // The series of the requests, in a fixed order.
        let requests = |metrics: &str| {
            let requests = metrics
                .lines()
                .filter(|line| line.starts_with("zkboost_http_requests_total"));
            let mut requests = Vec::from_iter(requests.map(str::to_owned));
            requests.sort();
            requests
        };
        let before = requests(&self.metrics().await);
        let expected = match self.scenario.el_unreachable {
            true => (StatusCode::BAD_GATEWAY, String::new()),
            false => self.post(&self.el, body, jwt).await,
        };
        assert_eq!(
            self.post(&self.zkboost, body, jwt).await,
            expected,
            "{body}"
        );

        let after = requests(&self.metrics().await);
        let Some(method) = counted else {
            return assert_eq!(after, before, "{body}");
        };
        let status = expected.0.as_u16();
        let prefix = format!(
            r#"zkboost_http_requests_total{{endpoint="{method}",method="POST",status="{status}"}} "#
        );
        let count = |requests: &[String]| {
            let mut counts = requests
                .iter()
                .filter_map(|line| line.strip_prefix(&prefix));
            counts
                .next()
                .map_or(0, |count| count.parse::<u64>().unwrap())
        };
        assert_eq!(count(&after), count(&before) + 1, "{body}");
    }

    /// Checks what holds at the end of every case, for each prover.
    ///
    /// - The histograms and the gauges agree with the counters of the outcome.
    /// - The steps of the accepted proofs add up to their latency from the slot start, apart from
    ///   the input building.
    /// - The dashboard shows the result of every proof generation.
    /// - The trace of a proof generation joins the trace of the CL. `request_proof` has the
    ///   attributes that the Grafana trace table selects, and the prove span as a child.
    /// - A beacon block root that zkboost has, from an event or from a lookup, is not looked up
    ///   again. So each prover lists the children of the parent one time at most.
    async fn check_invariants(&self) {
        let metrics = self.metrics().await;
        let value = |name: &str| series_value(&metrics, name);
        let url = self.zkboost.join("dashboard/state").unwrap();
        let state: Value = (self.http.get(url).send().await.unwrap())
            .json()
            .await
            .unwrap();
        let blocks = state["historicalBlocks"].as_array().unwrap();

        let outcomes = self.outcomes().await;
        let payloads = outcomes[0].valid_payloads + outcomes[0].invalid_payloads;
        let witnesses = outcomes[0].witnesses;
        let latest = [0, TIMESTAMP][usize::from(outcomes[0].valid_payloads > 0)];
        let no_submission_errors = (outcomes.iter()).all(|outcome| outcome.submission_errors == 0);
        for (series, expected) in [
            ("zkboost_payload_arrival_delay_seconds_count", payloads),
            ("zkboost_latest_payload_timestamp_seconds", latest),
            (
                "zkboost_programs_loaded",
                self.scenario.provers.len() as u64,
            ),
            ("zkboost_witness_fetch_duration_seconds_count", witnesses),
            (
                "zkboost_witness_bytes_sum",
                witnesses * WITNESS.len() as u64,
            ),
        ] {
            assert_eq!(value(series), expected as f64, "{series}");
        }

        for (prover, outcome) in self.scenario.provers.iter().zip(outcomes) {
            let of_prover = |name: &str| value(&format!(r#"{name}{{proof_type="{prover}"}}"#));
            for (series, expected) in [
                ("zkboost_queue_depth", 0),
                ("zkboost_prove_duration_seconds_count", outcome.proved),
                ("zkboost_prove_proof_bytes_sum", outcome.proved * PROOF_SIZE),
                (
                    "zkboost_submission_duration_seconds_count",
                    outcome.submitted,
                ),
                ("zkboost_proof_latency_seconds_count", outcome.accepted),
            ] {
                assert_eq!(of_prover(series), expected as f64, "{prover} {series}");
            }

            let steps = [
                "payload_arrival",
                "witness_fetch",
                "queue_wait",
                "prove",
                "submission",
            ]
            .map(|step| {
                value(&format!(
                    r#"zkboost_proof_step_seconds_sum{{proof_type="{prover}",step="{step}"}}"#
                ))
            });
            let (step_sum, slot_latency) = (
                steps.iter().sum::<f64>(),
                of_prover("zkboost_proof_slot_latency_seconds_sum"),
            );
            assert!(
                step_sum <= slot_latency && slot_latency - step_sum < 1.0,
                "{step_sum} {slot_latency}"
            );

            for (result, proofs) in [
                ("success", outcome.proved),
                ("error", outcome.prove_errors),
                ("timeout", outcome.prove_timeouts),
            ] {
                let shown = blocks
                    .iter()
                    .filter(|block| block["proofs"][prover]["result"] == result);
                assert_eq!(shown.count() as u64, proofs, "{prover} {result}");
            }
            if outcome.proved + outcome.prove_errors + outcome.prove_timeouts > 0 {
                self.check_trace(prover).await;
            }
        }
        if no_submission_errors {
            let child_lists = self.fakes.child_lists.load(Ordering::SeqCst);
            assert!(child_lists <= self.scenario.provers.len(), "{child_lists}");
        }
    }

    async fn check_trace(&self, prover: &str) {
        // The spans end shortly after the proof task.
        let deadline = Instant::now() + Duration::from_secs(5);
        let (request, request_proof) = loop {
            let mut spans = SPANS.0.get_finished_spans().unwrap();
            spans.retain(|span| span.span_context.trace_id().to_string() == self.trace_id);
            let parent = |span: &SpanData| {
                let is_parent =
                    |parent: &&SpanData| parent.span_context.span_id() == span.parent_span_id;
                spans.iter().find(is_parent)
            };
            let prove = (spans.iter()).find(|span| span.name == format!("prove/{prover}"));
            let request_proof = prove.and_then(parent);
            if let Some(request) = request_proof.and_then(parent) {
                break (request.clone(), request_proof.unwrap().clone());
            }
            assert!(Instant::now() < deadline, "no trace of prove/{prover}");
            sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(request.name, "request");
        assert_eq!(request.parent_span_id.to_string(), CL_SPAN_ID);
        assert_eq!(request_proof.name, "request_proof");
        // An unsigned integer field is exported as a string.
        for (key, value) in [
            ("block_number", BLOCK_NUMBER),
            ("gas_used", GAS_USED),
            ("timestamp", TIMESTAMP),
        ] {
            let mut attributes = request_proof.attributes.iter();
            let attribute = attributes.find(|attribute| attribute.key.as_str() == key);
            assert_eq!(
                attribute.unwrap().value.as_str(),
                value.to_string(),
                "{key}"
            );
        }
    }
}

/// Returns the JSON-RPC request of a method without params.
fn call(method: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": [] }).to_string()
}

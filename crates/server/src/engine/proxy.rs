//! The Engine API proxy. Every request goes to the EL unchanged with the `Authorization` header
//! of the caller. An `engine_newPayloadV5` goes upstream as `engine_newPayloadWithWitnessV5`
//! while zkboost can prove, and its response returns without the witness.
//!
//! The proxy reads the compact JSON that the clients write as bytes, so it does not parse the
//! payload or the witness before the response. Any other form takes the full JSON parse.

use std::{borrow::Cow, collections::BTreeMap, sync::Arc, time::Instant};

use axum::{
    body::{Body, to_bytes},
    extract::State,
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use memchr::{memchr, memmem};
use serde_json::value::{RawValue, to_raw_value};
use tokio::task::spawn_blocking;
use tracing::{Instrument, Span, error, info_span, warn};
use url::Url;

use crate::{
    engine::payload::{NewPayloadResult, PayloadHeader, PayloadStatus, Receipt},
    observability::{
        metrics::{ProxyMetrics, WitnessFetchStatus},
        telemetry,
    },
    proof::dispatcher::ProofDispatcher,
};

const NEW_PAYLOAD: &str = "engine_newPayloadV5";

const NEW_PAYLOAD_WITH_WITNESS: &str = "engine_newPayloadWithWitnessV5";

const METHOD_NOT_FOUND: i64 = -32601;

/// The largest request body, which is the limit of the Engine API of geth and of reth.
const BODY_LIMIT: usize = 128 << 20;

/// A JSON object whose values stay raw, so a rewrite copies them without a decode.
type RawObject<'a> = BTreeMap<Cow<'a, str>, &'a RawValue>;

/// Returns the JSON-RPC method of a request body.
fn method(body: &[u8]) -> Option<&str> {
    compact_method(body).or_else(|| {
        let request = serde_json::from_slice::<RawObject<'_>>(body).ok()?;
        let method: &RawValue = request.get("method")?;
        serde_json::from_str(method.get()).ok()
    })
}

/// Finds the method among the first members of a compact request.
fn compact_method(body: &[u8]) -> Option<&str> {
    const KEY: &[u8] = b"\"method\":\"";
    let head = &body[..body.len().min(128)];
    let key = memmem::find(head, KEY)?;
    // The key is a member of the request itself only if no other value opens before it.
    let opened = head[..key]
        .iter()
        .filter(|byte| matches!(byte, b'{' | b'['));
    let name = &head[key + KEY.len()..];
    let name = &name[..memchr(b'"', name)?];
    let plain = body[0] == b'{' && opened.count() == 1 && !name.contains(&b'\\');
    plain.then(|| str::from_utf8(name).ok())?
}

/// Returns the request with another method. `method` is the name inside `body`.
fn with_method(body: &[u8], method: &str, replacement: &str) -> Bytes {
    let start = (method.as_ptr().addr()).wrapping_sub(body.as_ptr().addr());
    assert!(
        start + method.len() <= body.len(),
        "the method is a part of the body"
    );
    let after = &body[start + method.len()..];
    [&body[..start], replacement.as_bytes(), after]
        .concat()
        .into()
}

/// The parts of the response of the EL to an `engine_newPayloadWithWitness`.
struct WitnessResponse {
    status: PayloadStatus,
    /// The witness as a JSON string.
    witness: Option<Bytes>,
    /// The response without the witness.
    body: Bytes,
    method_not_found: bool,
}

impl WitnessResponse {
    fn parse(body: &Bytes) -> Option<Self> {
        if let Some((witness, rest)) = Self::split_compact_witness(body)
            && let Some(response) = Self::parse_json(&rest)
        {
            return Some(Self {
                witness: Some(witness),
                ..response
            });
        }
        Self::parse_json(body)
    }

    /// Cuts the witness member out of a compact response, and returns the witness and the rest.
    /// The rest is a response only if the cut was right, which the caller checks with a parse.
    fn split_compact_witness(body: &Bytes) -> Option<(Bytes, Bytes)> {
        const KEY: &[u8] = b"\"witness\":\"";
        let key = memmem::find(body, KEY)?;
        let value = key + KEY.len() - 1;
        let end = value + 2 + memchr(b'"', &body[value + 1..])?;
        // The member goes with the comma that separates it from a neighbor.
        let rest = match (&body[..key], &body[end..]) {
            (before, [b',', after @ ..]) | ([before @ .., b','], after) | (before, after) => {
                [before, after].concat()
            }
        };
        Some((body.slice(value..end), rest.into()))
    }

    fn parse_json(body: &Bytes) -> Option<Self> {
        let mut envelope: RawObject<'_> = serde_json::from_slice(body).ok()?;
        let error_code = envelope.get("error").and_then(|error| {
            let error = serde_json::from_str::<RawObject<'_>>(error.get()).ok()?;
            serde_json::from_str::<i64>(error.get("code")?.get()).ok()
        });
        let mut result = envelope
            .get("result")
            .and_then(|result| serde_json::from_str::<RawObject<'_>>(result.get()).ok())
            .unwrap_or_default();
        let status = result
            .get("status")
            .and_then(|status| serde_json::from_str(status.get()).ok())
            .unwrap_or(PayloadStatus::Error);
        let removed = result.remove("witness");
        let result = to_raw_value(&result).ok()?;
        let stripped = match removed {
            Some(_) => {
                envelope.insert("result".into(), &result);
                serde_json::to_vec(&envelope).ok()?.into()
            }
            None => body.clone(),
        };
        Some(Self {
            status,
            witness: removed
                .filter(|witness| witness.get() != "null")
                .map(|witness| body.slice_ref(witness.get().as_bytes())),
            body: stripped,
            method_not_found: error_code == Some(METHOD_NOT_FOUND),
        })
    }
}

struct ElResponse {
    status: StatusCode,
    body: Bytes,
}

pub(crate) struct Proxy {
    el: Url,
    http: reqwest::Client,
    metrics: ProxyMetrics,
    pub(crate) dispatcher: Arc<ProofDispatcher>,
}

impl Proxy {
    pub(crate) fn new(el: Url, dispatcher: Arc<ProofDispatcher>) -> Self {
        Self {
            el,
            http: reqwest::Client::new(),
            metrics: ProxyMetrics::new(),
            dispatcher,
        }
    }

    /// Handles `POST /`. For an unreachable EL, the response is 502. A request is recorded under
    /// its JSON-RPC method unless the EL rejects its JWT, so a caller without the JWT adds no
    /// series while the EL responds.
    pub(crate) async fn handle(
        State(proxy): State<Arc<Self>>,
        headers: HeaderMap,
        body: Body,
    ) -> Response {
        // Both include the upload of the body, so they start before it.
        let _in_flight = proxy.metrics.in_flight();
        let started = Instant::now();
        let Ok(body) = to_bytes(body, BODY_LIMIT).await else {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        };
        let authorization = headers.get(AUTHORIZATION);
        let method = method(&body);
        let span = info_span!("request", method, otel.kind = "server");
        telemetry::set_remote_parent(&span, &headers);

        let el_response = match method {
            Some(method) if method == NEW_PAYLOAD => {
                proxy
                    .new_payload(authorization, &body, method)
                    .instrument(span.clone())
                    .await
            }
            _ => proxy.forward(authorization, body.clone()).await,
        };
        let response = match el_response {
            Ok(ElResponse { status, body }) => {
                (status, [(CONTENT_TYPE, "application/json")], body).into_response()
            }
            Err(error) => {
                error!(parent: &span, %error, "el unreachable");
                StatusCode::BAD_GATEWAY.into_response()
            }
        };
        let status = response.status();
        if let Some(method) = method
            && !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        {
            proxy.metrics.request(method, status, started.elapsed());
        }
        response
    }

    async fn forward(
        &self,
        authorization: Option<&HeaderValue>,
        body: Bytes,
    ) -> reqwest::Result<ElResponse> {
        let mut request = self
            .http
            .post(self.el.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization);
        }
        let response = request.send().await?;
        Ok(ElResponse {
            status: response.status(),
            body: response.bytes().await?,
        })
    }

    /// Forwards a new payload and responds to the caller. Every other work on the payload runs
    /// in the background, after the response. `method` is the method name inside `body`.
    async fn new_payload(
        &self,
        authorization: Option<&HeaderValue>,
        body: &Bytes,
        method: &str,
    ) -> reqwest::Result<ElResponse> {
        let receipt = Receipt::now();
        let header = spawn_blocking({
            let body = body.clone();
            move || PayloadHeader::decode(&body)
        });
        let (response, result) = if self.dispatcher.ready() {
            self.forward_with_witness(authorization, body, method, receipt.monotonic)
                .await
        } else {
            self.forward_without_witness(authorization, body).await
        };
        let dispatcher = self.dispatcher.clone();
        self.dispatcher.spawn(dispatcher.on_new_payload_response(
            header,
            result,
            receipt,
            Span::current(),
        ));
        response
    }

    async fn forward_without_witness(
        &self,
        authorization: Option<&HeaderValue>,
        body: &Bytes,
    ) -> (reqwest::Result<ElResponse>, NewPayloadResult) {
        let response = self.forward(authorization, body.clone()).await;
        let status = response
            .as_ref()
            .ok()
            .and_then(|response| WitnessResponse::parse(&response.body))
            .map_or(PayloadStatus::Error, |parsed| parsed.status);
        let result = NewPayloadResult {
            status,
            witness: None,
            witness_fetch: None,
        };
        (response, result)
    }

    /// Forwards the payload with the witness method, or without it to an EL that does not serve
    /// that method.
    async fn forward_with_witness(
        &self,
        authorization: Option<&HeaderValue>,
        body: &Bytes,
        method: &str,
        received: Instant,
    ) -> (reqwest::Result<ElResponse>, NewPayloadResult) {
        let witness_request = with_method(body, method, NEW_PAYLOAD_WITH_WITNESS);
        let response = self.forward(authorization, witness_request).await;
        let duration = received.elapsed();
        let parsed = response
            .as_ref()
            .ok()
            .and_then(|response| WitnessResponse::parse(&response.body));
        if parsed
            .as_ref()
            .is_some_and(|parsed| parsed.method_not_found)
        {
            warn!("witness method not served by the el, forwarded without proving");
            self.metrics
                .witness_fetched(WitnessFetchStatus::Missing, duration, 0);
            return self.forward_without_witness(authorization, body).await;
        }

        let status = parsed
            .as_ref()
            .map_or(PayloadStatus::Error, |parsed| parsed.status);
        // Only a VALID status comes with a witness.
        let witness = parsed
            .as_ref()
            .and_then(|parsed| parsed.witness.clone())
            .filter(|_| status == PayloadStatus::Valid);
        let fetch_status = match (status, &witness) {
            (PayloadStatus::Valid, Some(_)) => WitnessFetchStatus::Success,
            (PayloadStatus::Valid, None) => WitnessFetchStatus::Missing,
            (PayloadStatus::Error, _) => WitnessFetchStatus::Error,
            _ => WitnessFetchStatus::NotValid,
        };
        let response = response.map(|response| ElResponse {
            status: response.status,
            body: parsed.map_or(response.body, |parsed| parsed.body),
        });
        let result = NewPayloadResult {
            status,
            witness,
            witness_fetch: Some(duration),
        };
        self.metrics
            .witness_fetched(fetch_status, duration, result.witness_size());
        (response, result)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
        hint::black_box,
        time::Instant,
    };

    use alloy_primitives::hex;
    use bytes::Bytes;
    use serde_json::{Value, json};

    use crate::engine::{
        payload::PayloadStatus::{self, Accepted, Error, Invalid, Syncing, Valid},
        proxy::{NEW_PAYLOAD_WITH_WITNESS, WitnessResponse, method, with_method},
    };

    /// Counts the allocations of the current thread and their bytes.
    struct CountingAllocator;

    thread_local! {
        static ALLOCATED: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ALLOCATED.with(|allocated| {
                let (count, bytes) = allocated.get();
                allocated.set((count + 1, bytes + layout.size()));
            });
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    /// Returns the result of `run` with the count and the bytes of its allocations.
    fn allocations<T>(run: impl FnOnce() -> T) -> (T, usize, usize) {
        let before = ALLOCATED.with(Cell::get);
        let result = run();
        let after = ALLOCATED.with(Cell::get);
        (result, after.0 - before.0, after.1 - before.1)
    }

    /// The request and the response of the fixture payload, as a CL and an EL write them.
    fn new_payload() -> (Bytes, Bytes) {
        let params = include_str!("../../tests/fixture/new_payload_params_amsterdam.json");
        let witness = include_bytes!("../../tests/fixture/witness_amsterdam.rlp");
        let request = format!(
            r#"{{"jsonrpc":"2.0","method":"engine_newPayloadV5","params":{params},"id":1}}"#
        );
        let response = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{"status":"VALID","witness":"{}","latestValidHash":null}}}}"#,
            hex::encode_prefixed(witness)
        );
        (request.into(), response.into())
    }

    /// The method is the one of the request itself, in a compact form or any other.
    #[test]
    fn test_method() {
        for body in [
            r#"{"jsonrpc":"2.0","id":1,"method":"engine_x","params":[]}"#,
            r#"{"jsonrpc":"2.0","method":"engine_x","params":[],"id":1}"#,
            r#"{ "jsonrpc": "2.0", "method": "engine_x", "params": [] }"#,
            r#"{"params":[{"method":"engine_y"}],"method":"engine_x"}"#,
            r#"{"id":{"method":"engine_y"},"method":"engine_x"}"#,
            r#"{"id":"{","method":"engine_x"}"#,
        ] {
            assert_eq!(method(body.as_bytes()), Some("engine_x"), "{body}");
        }
        for body in [
            r#"{"params":[{"method":"engine_y"}]}"#,
            r#"{"method":"engine_\u0078"}"#,
            r#"[{"method":"engine_x"}]"#,
            "not json",
            "",
        ] {
            assert_eq!(method(body.as_bytes()), None, "{body}");
        }
        let body = br#"{"id":1,"method":"engine_newPayloadV5","params":[]}"#;
        assert_eq!(
            with_method(body, method(body).unwrap(), NEW_PAYLOAD_WITH_WITNESS),
            r#"{"id":1,"method":"engine_newPayloadWithWitnessV5","params":[]}"#
        );
    }

    /// A response keeps every field but the witness, and has a status only with a known one.
    #[test]
    fn test_witness_response() {
        let cases: [(Value, PayloadStatus, bool); 7] = [
            (
                json!({"status": "VALID", "witness": "0xc0", "latestValidHash": "0x01"}),
                Valid,
                true,
            ),
            (
                json!({"status": "VALID", "validationError": null}),
                Valid,
                false,
            ),
            (json!({"status": "VALID", "witness": null}), Valid, false),
            (
                json!({"status": "INVALID", "validationError": "bad \"witness\":\"0x\""}),
                Invalid,
                false,
            ),
            (json!({"status": "SYNCING"}), Syncing, false),
            (json!({"status": "ACCEPTED"}), Accepted, false),
            (json!({"status": "UNKNOWN", "witness": "0xc0"}), Error, true),
        ];
        for (result, status, witness) in cases {
            let el_response = json!({"jsonrpc": "2.0", "id": 7, "result": result});
            let mut expected = el_response.clone();
            let fields = expected["result"].as_object_mut().unwrap();
            fields.remove("witness");
            // The compact form takes the byte path, and the pretty form the full parse.
            for body in [el_response.to_string(), format!("{el_response:#}")] {
                let response = WitnessResponse::parse(&Bytes::from(body.clone())).unwrap();
                assert_eq!(response.status, status, "{body}");
                let expected_witness = witness.then_some(&b"\"0xc0\""[..]);
                assert_eq!(response.witness.as_deref(), expected_witness, "{body}");
                assert!(!response.method_not_found);
                let stripped: Value = serde_json::from_slice(&response.body).unwrap();
                assert_eq!(stripped, expected, "{body}");
            }
        }

        let error = json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32601, "message": "x"}});
        let body = Bytes::from(error.to_string());
        let response = WitnessResponse::parse(&body).unwrap();
        assert!(response.method_not_found && response.status == Error);
        assert_eq!(response.body, body);
        assert!(WitnessResponse::parse(&Bytes::from_static(b"not json")).is_none());

        // A key with an escape does not keep the witness in the response.
        let escaped = r#"{"id":7,"res\u0075lt":{"stat\u0075s":"VALID","witn\u0065ss":"0xc0"}}"#;
        let response = WitnessResponse::parse(&Bytes::from_static(escaped.as_bytes())).unwrap();
        assert!(response.status == Valid && response.witness.is_some());
        assert_eq!(response.body, r#"{"id":7,"result":{"status":"VALID"}}"#);
    }

    /// The proxy reads a new payload and its response without a parse of the payload or of the
    /// witness. It copies the request one time, and no part of the witness.
    #[test]
    fn test_new_payload_allocations() {
        let (request, response) = new_payload();

        let (name, count, _) = allocations(|| method(&request).unwrap());
        assert_eq!((name, count), ("engine_newPayloadV5", 0));

        let (witness_request, count, bytes) =
            allocations(|| with_method(&request, name, NEW_PAYLOAD_WITH_WITNESS));
        assert_eq!(method(&witness_request), Some(NEW_PAYLOAD_WITH_WITNESS));
        assert_eq!((count, bytes), (1, witness_request.len()));

        let (parsed, _, bytes) = allocations(|| WitnessResponse::parse(&response).unwrap());
        assert_eq!(
            parsed.witness.unwrap().len(),
            response.len() - parsed.body.len() - 11
        );
        assert!(bytes < 4096, "{bytes}");
    }

    /// Prints the time of each step of the proxy on a new payload.
    #[test]
    #[ignore = "a benchmark, run it with --release -- --ignored --nocapture"]
    fn bench_new_payload() {
        let (request, response) = new_payload();
        let time = |name: &str, run: &dyn Fn() -> usize| {
            let rounds = 500;
            (0..rounds).for_each(|_| _ = black_box(run()));
            let started = Instant::now();
            (0..rounds).for_each(|_| _ = black_box(run()));
            println!("{name}: {} ns", started.elapsed().as_nanos() / rounds);
        };
        println!(
            "request {} bytes, response {} bytes",
            request.len(),
            response.len()
        );
        time("method", &|| method(&request).unwrap().len());
        time("with_method", &|| {
            with_method(
                &request,
                method(&request).unwrap(),
                NEW_PAYLOAD_WITH_WITNESS,
            )
            .len()
        });
        time("WitnessResponse::parse", &|| {
            WitnessResponse::parse(&response).unwrap().body.len()
        });
    }
}

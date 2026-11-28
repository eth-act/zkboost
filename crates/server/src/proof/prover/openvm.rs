//! Client of the axiom-edge manager of an OpenVM proving cluster. It needs the
//! [han0110/axiom-edge](https://github.com/han0110/axiom-edge) fork, which serves a proof at
//! `GET /proof/{proof_uuid}`.

use std::{sync::Arc, time::Duration};

use alloy_primitives::hex;
use anyhow::{Context, bail, ensure};
use bytes::Bytes;
use rand::{Rng, rng};
use reqwest::{
    RequestBuilder, Response, StatusCode,
    multipart::{Form, Part},
};
use reqwest_eventsource::{Error as EventSourceError, Event, EventSource};
use serde::{Deserialize, Serialize, de::IgnoredAny};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::time::sleep;
use tokio_stream::StreamExt;
use tokio_util::task::TaskTracker;
use tracing::{Span, info, warn};

use crate::{
    proof::prover::{CancelOnDrop, Turn},
    util::error_for_status,
};

/// Bound on the loadout read, which the proof timeout does not cover.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Attempts to start a proof, `START_PROOF_RETRY_DELAY` apart. The manager responds with 409 while
/// the previous proof still holds its one proof slot, for example while its cancel is on the way.
const START_PROOF_ATTEMPTS: u32 = 5;

const START_PROOF_RETRY_DELAY: Duration = Duration::from_millis(100);

pub(crate) struct OpenvmCluster {
    http: reqwest::Client,
    endpoint: String,
    program: Program,
}

/// A guest of the loadout of the manager. A guest is named `program-` and the first 8 bytes of
/// the SHA-256 of its ELF in hex, at version 0, as provoor deploys it.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Program {
    name: String,
    version: u32,
}

#[derive(Deserialize)]
struct Loadout {
    programs: Vec<Program>,
}

/// The data of a `status` event of `GET /proof_events/{proof_uuid}`.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProofStatus {
    InProgress,
    Completed,
    Failing(IgnoredAny),
    Failed(String),
    Canceled,
}

impl OpenvmCluster {
    /// Connects to the manager and checks that its loadout holds the guest `elf`.
    pub(crate) async fn new(endpoint: &str, elf: &[u8]) -> anyhow::Result<Self> {
        let cluster = Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.trim_end_matches('/').to_owned(),
            program: Program {
                name: format!("program-{}", hex::encode(&Sha256::digest(elf)[..8])),
                version: 0,
            },
        };
        let request = cluster
            .http
            .get(cluster.url("loadout"))
            .timeout(REQUEST_TIMEOUT);
        let loadout: Loadout = send(request).await?.json().await?;
        ensure!(
            loadout.programs.contains(&cluster.program),
            "{} is not in the loadout {:?}",
            cluster.program.name,
            loadout.programs
        );
        Ok(cluster)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.endpoint)
    }

    /// Generates the stark proof of `stdin`, encoded with the codec that the ere verifier reads.
    pub(crate) async fn prove(
        self: &Arc<Self>,
        stdin: &[u8],
        tasks: &TaskTracker,
        turn: Turn,
    ) -> anyhow::Result<Bytes> {
        let job_id = format!("{:032x}", rng().random::<u128>());
        // The guard exists before the submission, so an abandoned submission is cancelled too.
        let mut cancel = CancelOnDrop::new(tasks, turn, {
            let (cluster, job_id) = (self.clone(), job_id.clone());
            async move {
                let body = json!({ "proof_uuid": job_id });
                let request = cluster.http.post(cluster.url("cancel_proof")).json(&body);
                send(request).await.map(|_| ())
            }
        });

        let input = Part::bytes(encode_stdin(stdin)).file_name("input.bin");
        let upload = self.http.post(self.url(&format!("upload_input/{job_id}")));
        send(upload.multipart(Form::new().part("input", input)))
            .await
            .context("upload input")?;
        self.start_proof(&job_id).await.context("start proof")?;
        Span::current().record("job_id", job_id.as_str());
        info!(%job_id, "openvm cluster prove job created");

        let status = self.settled_status(&job_id).await?;
        cancel.disarm();
        match status {
            ProofStatus::Completed => {
                let proof = send(self.http.get(self.url(&format!("proof/{job_id}")))).await?;
                Ok(proof.bytes().await?)
            }
            ProofStatus::Failed(reason) => bail!("openvm prove job failed: {reason}"),
            _ => bail!("openvm prove job cancelled"),
        }
    }

    async fn start_proof(&self, job_id: &str) -> anyhow::Result<()> {
        let body = json!({ "proof_uuid": job_id, "program": self.program, "proof_type": "stark" });
        for attempt in 1.. {
            let response = self
                .http
                .post(self.url("start_proof"))
                .json(&body)
                .send()
                .await?;
            if response.status() != StatusCode::CONFLICT || attempt == START_PROOF_ATTEMPTS {
                error_for_status(response).await?;
                break;
            }
            warn!(%job_id, attempt, "openvm cluster prove job start retried");
            sleep(START_PROOF_RETRY_DELAY).await;
        }
        Ok(())
    }

    /// Follows the status events of a prove job until it settles.
    async fn settled_status(&self, job_id: &str) -> anyhow::Result<ProofStatus> {
        let request = self.http.get(self.url(&format!("proof_events/{job_id}")));
        let mut events = EventSource::new(request).expect("the request has no body");
        loop {
            match events.next().await.context("proof events closed")? {
                Ok(Event::Message(message)) if message.event == "status" => {
                    match serde_json::from_str(&message.data).context("decode proof status")? {
                        ProofStatus::InProgress | ProofStatus::Failing(_) => {}
                        settled => return Ok(settled),
                    }
                }
                // The event source reconnects a stream that ends before the settled status, and
                // the manager then sends the current status again.
                Ok(_) | Err(EventSourceError::StreamEnded) => {}
                Err(error) => return Err(error).context("read proof events"),
            }
        }
    }
}

async fn send(request: RequestBuilder) -> anyhow::Result<Response> {
    error_for_status(request.send().await?).await
}

/// Encodes `stdin` as the bincode of the `StdIn` of the OpenVM SDK, one buffered byte string and
/// no deferrals. Every length is a little-endian `u64`.
fn encode_stdin(stdin: &[u8]) -> Vec<u8> {
    [
        &1u64.to_le_bytes(),
        &(stdin.len() as u64).to_le_bytes(),
        stdin,
        &0u64.to_le_bytes(),
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };

    use axum::{Router, http::StatusCode, routing::post};
    use tokio::{net::TcpListener, sync::mpsc};
    use tokio_util::task::TaskTracker;

    use crate::proof::prover::{
        Turn,
        openvm::{OpenvmCluster, Program, START_PROOF_ATTEMPTS},
    };

    /// A start that the manager responds to with 409 is retried `START_PROOF_ATTEMPTS` times, and
    /// an abandoned job is cancelled. Its turn ends after the cancel.
    #[tokio::test]
    async fn test_start_proof_retries_conflict() {
        let (starts, cancels) = <(Arc<AtomicU32>, Arc<AtomicU32>)>::default();
        let count = |counter: &Arc<AtomicU32>, status| {
            let counter = counter.clone();
            post(move || async move {
                counter.fetch_add(1, Ordering::SeqCst);
                status
            })
        };
        let manager = Router::new()
            .route("/upload_input/{uuid}", post(StatusCode::OK))
            .route("/start_proof", count(&starts, StatusCode::CONFLICT))
            .route("/cancel_proof", count(&cancels, StatusCode::OK));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cluster = Arc::new(OpenvmCluster {
            http: reqwest::Client::new(),
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
            program: Program {
                name: "program-test".to_owned(),
                version: 0,
            },
        });
        tokio::spawn(async move { axum::serve(listener, manager).await });

        let tasks = TaskTracker::new();
        let (idle_tx, mut idle_rx) = mpsc::unbounded_channel();
        let proof = cluster.prove(&[1], &tasks, Turn(idle_tx)).await;
        assert!(proof.is_err() && idle_rx.try_recv().is_err());
        tasks.close();
        tasks.wait().await;
        assert!(idle_rx.try_recv().is_ok());
        assert_eq!(starts.load(Ordering::SeqCst), START_PROOF_ATTEMPTS);
        assert_eq!(cancels.load(Ordering::SeqCst), 1);
    }
}

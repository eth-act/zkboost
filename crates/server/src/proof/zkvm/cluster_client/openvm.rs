//! HTTP client of the axiom-edge manager of an OpenVM proving cluster.
//!
//! The client targets the [han0110/axiom-edge](https://github.com/han0110/axiom-edge) fork. The
//! upstream manager has no `GET /proof/{proof_uuid}` route to download a proof.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, anyhow, bail, ensure};
use ere_server_client::Input;
use rand::{Rng, rng};
use reqwest::multipart::{Form, Part};
use reqwest_eventsource::{Event, EventSource};
use serde::{Deserialize, Serialize, de::IgnoredAny};
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;
use tracing::{Span, info, warn};

/// Bound on the requests outside the prove timeout of the worker, the loadout read and the
/// cancel.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A client of the axiom-edge manager of an OpenVM proving cluster.
///
/// The manager holds the guest programs of its loadout by name and version. A guest is named
/// `program-` and the first 8 bytes of the SHA-256 of its ELF in hex, at version 0, as provoor
/// deploys it.
#[derive(Debug)]
pub(crate) struct OpenVMClusterClient {
    http_client: reqwest::Client,
    endpoint: String,
    program: ProgramRef,
}

impl OpenVMClusterClient {
    /// Connects to the manager at `endpoint` and checks that its loadout holds the guest `elf`.
    pub(super) async fn new(endpoint: &str, elf: &[u8]) -> anyhow::Result<Self> {
        let elf_digest = Sha256::digest(elf);
        let client = Self {
            http_client: reqwest::Client::new(),
            endpoint: endpoint.trim_end_matches('/').to_string(),
            program: ProgramRef {
                name: format!(
                    "program-{}",
                    alloy_primitives::hex::encode(&elf_digest[..8])
                ),
                version: 0,
            },
        };
        let request = client
            .http_client
            .get(format!("{}/loadout", client.endpoint))
            .timeout(REQUEST_TIMEOUT);
        let loadout: LoadoutResponse = send(request)
            .await
            .context("read loadout")?
            .json()
            .await
            .context("decode loadout")?;
        ensure!(
            loadout.programs.contains(&client.program),
            "program {} is not in the loadout {:?}",
            client.program.name,
            loadout.programs
        );
        Ok(client)
    }

    /// Submits a prove job for `input`, returning an [`OpenVMProveJob`] that drives it to
    /// completion. The id of the job is recorded in the `job_id` field of `prove_span`.
    pub(super) async fn create_prove_job(
        self: &Arc<Self>,
        input: &Input,
        prove_span: &Span,
    ) -> anyhow::Result<OpenVMProveJob> {
        let job_id = format!("{:032x}", rng().random::<u128>());
        // The guard exists before the submission, so a submission the caller abandons is
        // cancelled too and does not hold the one proof slot of the manager.
        let job = OpenVMProveJob {
            client: self.clone(),
            job_id: Some(job_id.clone()),
        };
        self.upload_input(&job_id, input)
            .await
            .context("upload openvm prove job input")?;
        self.start_proof(&job_id)
            .await
            .context("submit openvm prove job")?;
        prove_span.record("job_id", job_id.as_str());
        info!(%job_id, "openvm cluster prove job created");
        Ok(job)
    }

    /// Stages the stdin of a prove job at the manager.
    async fn upload_input(&self, job_id: &str, input: &Input) -> anyhow::Result<()> {
        let form = Form::new().part(
            "input",
            Part::bytes(encode_stdin(input.stdin())).file_name("input.bin"),
        );
        let request = self
            .http_client
            .post(format!("{}/upload_input/{job_id}", self.endpoint))
            .multipart(form);
        send(request).await?;
        Ok(())
    }

    /// Starts the stark proof of the staged stdin of a prove job.
    async fn start_proof(&self, job_id: &str) -> anyhow::Result<()> {
        let request = self
            .http_client
            .post(format!("{}/start_proof", self.endpoint))
            .json(&StartProofRequest {
                proof_uuid: job_id,
                program: &self.program,
                proof_type: "stark",
            });
        send(request).await?;
        Ok(())
    }

    /// Follows the status events of a prove job until it settles.
    async fn wait_prove_job(&self, job_id: &str) -> anyhow::Result<ProofStatus> {
        let request = self
            .http_client
            .get(format!("{}/proof_events/{job_id}", self.endpoint));
        let mut events = EventSource::new(request).expect("the stream request carries no body");
        loop {
            match events.next().await {
                Some(Ok(Event::Message(message))) if message.event == "status" => {
                    match serde_json::from_str(&message.data).context("decode proof status")? {
                        ProofStatus::InProgress | ProofStatus::Failing(_) => {}
                        settled => return Ok(settled),
                    }
                }
                Some(Ok(_)) => {}
                // The manager ends the stream right after the settled status. The event source
                // reconnects a stream lost before that by itself, and the manager then sends
                // the current status again.
                Some(Err(reqwest_eventsource::Error::StreamEnded)) => {}
                Some(Err(error)) => return Err(error).context("read proof events"),
                None => bail!("proof events closed"),
            }
        }
    }

    /// Downloads the proof of a completed prove job, encoded with the openvm codec that the
    /// ere verifier decodes.
    async fn download_proof(&self, job_id: &str) -> anyhow::Result<Vec<u8>> {
        let request = self
            .http_client
            .get(format!("{}/proof/{job_id}", self.endpoint));
        let proof = send(request).await?.bytes().await.context("read proof")?;
        Ok(proof.to_vec())
    }

    /// Cancels a prove job. The manager answers a settled or unknown job the same way.
    async fn cancel_prove_job(&self, job_id: &str) -> anyhow::Result<()> {
        let request = self
            .http_client
            .post(format!("{}/cancel_proof", self.endpoint))
            .json(&CancelProofRequest { proof_uuid: job_id })
            .timeout(REQUEST_TIMEOUT);
        send(request).await?;
        Ok(())
    }
}

/// A handle to an in-flight prove job.
///
/// Dropping the handle before the job settles cancels it at the manager, so an abandoned proof
/// does not hold the one proof slot of the manager.
#[derive(Debug)]
pub(crate) struct OpenVMProveJob {
    client: Arc<OpenVMClusterClient>,
    /// The job identifier, taken once the job settles.
    job_id: Option<String>,
}

impl OpenVMProveJob {
    /// Awaits the job and returns the encoded proof.
    pub(super) async fn wait(&mut self) -> anyhow::Result<Vec<u8>> {
        let job_id = self.job_id.as_ref().context("job_id not set")?;
        let proof = match self.client.wait_prove_job(job_id).await? {
            ProofStatus::Completed => self.client.download_proof(job_id).await,
            ProofStatus::Failed(reason) => Err(anyhow!("openvm prove job failed: {reason}")),
            ProofStatus::Canceled => Err(anyhow!("openvm prove job cancelled")),
            ProofStatus::InProgress | ProofStatus::Failing(_) => {
                unreachable!("wait_prove_job returns a settled status")
            }
        };
        self.job_id = None;
        proof
    }
}

impl Drop for OpenVMProveJob {
    fn drop(&mut self) {
        let Some(job_id) = self.job_id.take() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!(%job_id, "no runtime to cancel openvm cluster prove job");
            return;
        };
        let client = self.client.clone();
        handle.spawn(async move {
            if let Err(error) = client.cancel_prove_job(&job_id).await {
                warn!(%job_id, %error, "failed to cancel openvm cluster prove job");
            }
        });
    }
}

/// Sends a request to the manager. The body of an error answer is the reason.
async fn send(request: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
    let response = request.send().await?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(anyhow!("{status}: {body}"))
}

/// Encodes `stdin` as the bincode of the `StdIn` of the OpenVM SDK, one buffered byte string
/// and no deferrals. Every length is a little-endian `u64`.
fn encode_stdin(stdin: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(stdin.len() + 24);
    encoded.extend_from_slice(&1u64.to_le_bytes());
    encoded.extend_from_slice(&(stdin.len() as u64).to_le_bytes());
    encoded.extend_from_slice(stdin);
    encoded.extend_from_slice(&0u64.to_le_bytes());
    encoded
}

/// A guest program of the loadout of the manager.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct ProgramRef {
    name: String,
    version: u32,
}

/// Answer of `GET /loadout`.
#[derive(Debug, Deserialize)]
struct LoadoutResponse {
    programs: Vec<ProgramRef>,
}

/// Body of `POST /start_proof`.
#[derive(Debug, Serialize)]
struct StartProofRequest<'a> {
    proof_uuid: &'a str,
    program: &'a ProgramRef,
    proof_type: &'a str,
}

/// Status of a prove job, the data of a `status` event of `GET /proof_events/{proof_uuid}`. A
/// failing job settles as failed with its reason.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProofStatus {
    InProgress,
    Completed,
    Failing(IgnoredAny),
    Failed(String),
    Canceled,
}

/// Body of `POST /cancel_proof`.
#[derive(Debug, Serialize)]
struct CancelProofRequest<'a> {
    proof_uuid: &'a str,
}

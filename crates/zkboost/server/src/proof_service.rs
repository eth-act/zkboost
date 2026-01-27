//! Background proof generation service with webhook delivery.
//!
//! This module provides asynchronous proof generation that runs in background tasks.
//! When a proof request is received, it is queued and processed asynchronously.
//! Upon completion (success or failure), the result is POSTed to a configured webhook URL.

use std::time::{Duration, Instant};

use ere_zkvm_interface::{Input, Proof, ProofKind};
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use zkboost_types::{ProgramID, ProofGenId, ProofResult};

use crate::{app::zkVMInstance, metrics::record_prove};

/// Message sent to the proof service to request proof generation.
#[derive(Debug)]
pub(crate) struct ProofMessage {
    /// Unique identifier for tracking this proof generation request.
    pub proof_gen_id: ProofGenId,
    /// Input data for the zkVM program execution.
    pub input: Input,
}

/// Background service that processes proof generation requests asynchronously.
///
/// Each `ProofService` instance is associated with a specific program and runs
/// as a background task. It receives proof requests via an mpsc channel,
/// generates proofs using the configured zkVM instance, and delivers results
/// to a webhook URL with automatic retry on failure.
pub(crate) struct ProofService {
    /// Identifier of the program this service generates proofs for.
    program_id: ProgramID,
    /// zkVM instance used for proof generation.
    zkvm: zkVMInstance,
    /// Channel receiver for incoming proof requests.
    proof_rx: mpsc::Receiver<ProofMessage>,
    /// HTTP client for webhook delivery.
    http_client: Client,
    /// URL to POST proof results to upon completion.
    webhook_url: String,
}

impl ProofService {
    /// Creates a new proof service and returns the sender for submitting proof requests.
    ///
    /// The returned `mpsc::Sender` should be used to submit `ProofMessage`s for processing.
    /// The channel has a buffer capacity of 128 messages.
    pub(crate) fn new(
        program_id: ProgramID,
        zkvm: zkVMInstance,
        http_client: Client,
        webhook_url: String,
    ) -> (Self, mpsc::Sender<ProofMessage>) {
        let (proof_tx, proof_rx) = mpsc::channel(128);

        (
            Self {
                program_id,
                zkvm,
                proof_rx,
                http_client,
                webhook_url,
            },
            proof_tx,
        )
    }

    /// Starts the background proof generation service.
    ///
    /// This consumes the service and spawns a background task that continuously
    /// processes incoming proof requests until the channel is closed.
    pub(crate) fn start_service(mut self) {
        tokio::spawn(async move {
            tracing::info!(
                program_id = %self.program_id.0,
                "Proof generation service started"
            );

            while let Some(msg) = self.proof_rx.recv().await {
                self.process_proof(msg, &self.zkvm).await;
            }

            tracing::warn!(
                program_id = %self.program_id.0,
                "Proof generation service ended"
            );
        });
    }

    /// Processes a single proof generation request.
    ///
    /// Generates a compressed proof using the zkVM instance, records metrics,
    /// and delivers the result to the webhook URL.
    async fn process_proof(&self, msg: ProofMessage, zkvm: &zkVMInstance) {
        let start = Instant::now();

        let proof_gen_id = msg.proof_gen_id;
        let program_id = self.program_id.clone();

        let (public_values, proof, report, error) = zkvm
            .prove(msg.input, ProofKind::Compressed)
            .await
            .map_err(|error| format!("Failed to generate proof: {error}"))
            .and_then(|(public_values, proof, report)| match proof {
                Proof::Compressed(proof) => Ok((public_values, proof, report, None)),
                _ => Err(format!("Unexpected proof kind {:?}", proof.kind())),
            })
            .unwrap_or_else(|error| {
                (
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Some(error),
                )
            });
        let proving_time_ms = report.proving_time.as_millis();

        record_prove(&program_id.0, error.is_none(), start.elapsed(), proof.len());

        self.post_webhook(ProofResult {
            proof_gen_id,
            public_values,
            proof,
            proving_time_ms,
            error,
        })
        .await;
    }

    /// Posts the proof result to the configured webhook URL with retry logic.
    ///
    /// Attempts up to 3 times with exponential backoff (1s, 2s delays between attempts).
    /// Stops retrying early on client errors (4xx status codes).
    async fn post_webhook(&self, proof_result: ProofResult) {
        const MAX_ATTEMPT: u32 = 3;

        for attempt in 1..=MAX_ATTEMPT {
            match self
                .http_client
                .post(&self.webhook_url)
                .json(&proof_result)
                .timeout(Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    info!(
                        proof_gen_id = %proof_result.proof_gen_id,
                        attempt = attempt,
                        "Successfully pushed proof_result to webhook url"
                    );
                    return;
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    error!(
                        proof_gen_id = %proof_result.proof_gen_id,
                        attempt = attempt,
                        status = %status,
                        body = %body,
                        "webhook url rejected proof_result push"
                    );

                    if status.is_client_error() {
                        return;
                    }
                }
                Err(e) => {
                    warn!(
                        proof_gen_id = %proof_result.proof_gen_id,
                        attempt = attempt,
                        error = %e,
                        "Failed to push proof_result to webhook url"
                    );
                }
            }

            if attempt < 3 {
                let delay = Duration::from_secs(2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }
        }

        error!(
            proof_gen_id = %proof_result.proof_gen_id,
            webhook_url = %self.webhook_url,
            "Failed to push proof to webhook url after 3 attempts"
        );
    }
}

use std::time::{Duration, Instant};

use ere_zkvm_interface::{Input, Proof, ProofKind};
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use zkboost_types::{ProgramID, ProofGenId, ProofResult};

use crate::{app::zkVMInstance, metrics::record_prove};

#[derive(Debug)]
pub(crate) struct ProofMessage {
    pub proof_gen_id: ProofGenId,
    pub input: Input,
}

pub(crate) struct ProofService {
    program_id: ProgramID,
    zkvm: zkVMInstance,
    proof_rx: mpsc::Receiver<ProofMessage>,
    http_client: Client,
    webhook_url: String,
}

impl ProofService {
    pub(crate) fn new(
        program_id: ProgramID,
        zkvm: zkVMInstance,
        http_client: Client,
        webhook_url: String,
    ) -> (Self, mpsc::Sender<ProofMessage>) {
        let (proof_tx, proof_rx) = mpsc::channel(100);

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

        record_prove(&program_id.0, error.is_none(), start.elapsed(), proof.len());

        self.post_webhook(ProofResult {
            proof_gen_id,
            public_values,
            proof,
            proving_time_ms: report.proving_time.as_millis(),
            error,
        })
        .await;
    }

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

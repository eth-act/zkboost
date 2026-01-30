// A lightweight mock implementation of the zkVM trait that can be used for unit tests.

use std::time::Duration;

use ere_zkvm_interface::{
    Input, ProgramExecutionReport, ProgramProvingReport, Proof, ProofKind, PublicValues,
};

#[derive(Clone)]
pub(crate) struct MockzkVM {
    mock_proving_time: Duration,
    mock_proof_size: u64,
}

impl MockzkVM {
    /// Construct a `MockzkVM`.
    pub(crate) fn new(mock_proving_time_ms: u64, mock_proof_size: u64) -> Self {
        Self {
            mock_proving_time: Duration::from_millis(mock_proving_time_ms),
            mock_proof_size,
        }
    }

    // Generate random proof.
    pub(crate) fn random_proof(&self) -> Vec<u8> {
        let mut proof = vec![0; self.mock_proof_size as usize];
        rand::fill(proof.as_mut_slice());
        proof
    }

    pub(crate) async fn execute(
        &self,
        _: &Input,
    ) -> anyhow::Result<(PublicValues, ProgramExecutionReport)> {
        // Simulate some computation time to avoid 0-ms durations
        let execution_duration = Duration::from_millis(10);
        tokio::time::sleep(execution_duration).await;
        Ok((
            Vec::new(),
            ProgramExecutionReport {
                total_num_cycles: 100,
                region_cycles: Default::default(),
                execution_duration,
            },
        ))
    }

    pub(crate) async fn prove(
        &self,
        _: &Input,
        proof_kind: ProofKind,
    ) -> anyhow::Result<(PublicValues, Proof, ProgramProvingReport)> {
        // Simulate some computation time to avoid 0-ms durations
        tokio::time::sleep(self.mock_proving_time).await;
        Ok((
            Vec::new(),
            Proof::new(proof_kind, self.random_proof()),
            ProgramProvingReport {
                proving_time: self.mock_proving_time,
            },
        ))
    }

    pub(crate) async fn verify(&self, proof: &Proof) -> anyhow::Result<PublicValues> {
        // Simulate some computation time to avoid 0-ms durations
        let verify_time = Duration::from_millis(10);
        tokio::time::sleep(verify_time).await;
        if proof.as_bytes().len() == self.mock_proof_size as usize {
            Ok(Vec::new())
        } else {
            anyhow::bail!("invalid proof")
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::HashMap, sync::Arc};

    use metrics_exporter_prometheus::PrometheusBuilder;
    use reqwest::Client;
    use zkboost_types::ProgramID;

    use crate::{
        app::{AppState, zkVMInstance},
        mock::MockzkVM,
        proof_service::ProofService,
    };

    /// Create an AppState with an optional mock program for testing.
    pub(crate) fn mock_app_state(program_id: Option<&ProgramID>) -> AppState {
        let http_client = Client::new();
        let mut proof_txs = HashMap::new();
        let mut programs = HashMap::new();

        if let Some(program_id) = program_id {
            let mock_proving_time_ms = 10;
            let mock_proof_size = 32;
            let zkvm = zkVMInstance::Mock(MockzkVM::new(mock_proving_time_ms, mock_proof_size));
            let (proof_service, proof_tx) = ProofService::new(
                program_id.clone(),
                zkvm.clone(),
                http_client.clone(),
                "http://localhost:3003/proofs".to_string(),
            );

            proof_service.start_service();

            proof_txs.insert(program_id.clone(), proof_tx);
            programs.insert(program_id.clone(), zkvm);
        }

        let recorder = PrometheusBuilder::new().build_recorder();
        AppState {
            programs: Arc::new(programs),
            proof_txs: Arc::new(proof_txs),
            metrics: recorder.handle(),
        }
    }
}

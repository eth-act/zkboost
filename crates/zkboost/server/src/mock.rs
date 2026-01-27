// A lightweight mock implementation of the zkVM trait that can be used for unit tests.

use std::{collections::HashMap, sync::Arc, time::Duration};

use ere_zkvm_interface::{
    Input, ProgramExecutionReport, ProgramProvingReport, Proof, ProofKind, PublicValues,
};
use metrics_exporter_prometheus::PrometheusBuilder;
use reqwest::Client;
use zkboost_types::ProgramID;

use crate::{
    app::{AppState, zkVMInstance},
    proof_service::ProofService,
};

#[derive(Clone)]
pub(crate) struct MockzkVM;

impl MockzkVM {
    pub(crate) fn execute(
        &self,
        _: &Input,
    ) -> anyhow::Result<(PublicValues, ProgramExecutionReport)> {
        // Simulate some computation time to avoid 0-ms durations in unit tests
        std::thread::sleep(Duration::from_millis(1));
        Ok((
            Vec::new(),
            ProgramExecutionReport {
                total_num_cycles: 100,
                region_cycles: Default::default(),
                execution_duration: Duration::from_millis(1),
            },
        ))
    }

    pub(crate) fn prove(
        &self,
        _: &Input,
        proof_kind: ProofKind,
    ) -> anyhow::Result<(PublicValues, Proof, ProgramProvingReport)> {
        // Simulate some computation time to avoid 0-ms durations in unit tests
        std::thread::sleep(Duration::from_millis(1));
        Ok((
            Vec::new(),
            Proof::new(proof_kind, b"mock_proof".to_vec()),
            ProgramProvingReport {
                proving_time: Duration::from_millis(1),
            },
        ))
    }

    pub(crate) fn verify(&self, proof: &Proof) -> anyhow::Result<PublicValues> {
        if proof.as_bytes() == b"mock_proof" {
            Ok(Vec::new())
        } else {
            anyhow::bail!("invalid proof")
        }
    }
}

/// Create an AppState with an optional mock program for testing.
pub(crate) fn mock_app_state(program_id: Option<&ProgramID>) -> AppState {
    let http_client = Client::new();
    let mut proof_txs = HashMap::new();
    let mut programs = HashMap::new();

    if let Some(program_id) = program_id {
        let zkvm = zkVMInstance::Mock(MockzkVM);
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

// A lightweight mock implementation of the zkVM trait that can be used for unit tests.

use std::{sync::Arc, time::Duration};

use ere_zkvm_interface::{
    Input, ProgramExecutionReport, ProgramProvingReport, Proof, ProofKind, PublicValues, zkVM,
};
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::sync::RwLock;
use zkboost_types::ProgramID;

use crate::app::{AppState, zkVMInstance};

pub(crate) struct MockzkVM;

impl zkVM for MockzkVM {
    fn execute(&self, _: &Input) -> anyhow::Result<(PublicValues, ProgramExecutionReport)> {
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

    fn prove(
        &self,
        _: &Input,
        proof_kind: ProofKind,
    ) -> anyhow::Result<(PublicValues, Proof, ProgramProvingReport)> {
        Ok((
            Vec::new(),
            Proof::new(proof_kind, b"mock_proof".to_vec()),
            ProgramProvingReport {
                proving_time: Duration::from_millis(1),
            },
        ))
    }

    fn verify(&self, proof: &Proof) -> anyhow::Result<PublicValues> {
        if proof.as_bytes() == b"mock_proof" {
            Ok(Vec::new())
        } else {
            anyhow::bail!("invalid proof")
        }
    }

    fn name(&self) -> &'static str {
        "mock"
    }

    fn sdk_version(&self) -> &'static str {
        "1.0.0"
    }
}

/// Create an AppState with an optional mock program for testing.
pub(crate) fn mock_app_state(program_id: Option<&ProgramID>) -> AppState {
    let programs = program_id
        .map(|id| FromIterator::from_iter([(id.clone(), zkVMInstance::new(Arc::new(MockzkVM)))]))
        .unwrap_or_default();
    let recorder = PrometheusBuilder::new().build_recorder();
    AppState {
        programs: Arc::new(RwLock::new(programs)),
        metrics: recorder.handle(),
    }
}

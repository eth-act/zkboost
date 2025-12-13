// A lightweight mock implementation of the zkVM trait that can be used for unit tests.

use std::{sync::Arc, time::Duration};

use ere_zkvm_interface::{
    Input, ProgramExecutionReport, ProgramProvingReport, Proof, ProofKind, PublicValues, zkVM,
};
use tokio::sync::RwLock;

use crate::common::{AppState, ProgramID, zkVMInstance};

#[derive(Default)]
pub struct MockZkVM;

impl zkVM for MockZkVM {
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

pub fn mock_app_state(program_id: &ProgramID) -> AppState {
    let programs =
        FromIterator::from_iter([(program_id.clone(), zkVMInstance::new(Arc::new(MockZkVM)))]);
    AppState {
        programs: Arc::new(RwLock::new(programs)),
    }
}

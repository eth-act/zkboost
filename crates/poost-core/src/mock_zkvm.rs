use std::time::Duration;
use ere_zkvm_interface::{Input, ProgramExecutionReport, ProgramProvingReport, Proof, ProofKind, PublicValues, zkVM};

#[derive(Default)]
pub struct MockZkVM;

impl zkVM for MockZkVM {
    fn execute(&self, _inputs: &Input) -> Result<(Vec<u8>, ProgramExecutionReport), anyhow::Error> {
        // Simulate some computation time to avoid 0-ms durations in unit tests
        std::thread::sleep(Duration::from_millis(1));
        Ok((vec![], ProgramExecutionReport {
            total_num_cycles: 100,
            region_cycles: Default::default(),
            execution_duration: Duration::from_millis(1),
        }))
    }

    fn prove(&self, _input: &Input,
    _proof_kind: ProofKind) -> Result<(Vec<u8>, Proof, ProgramProvingReport), anyhow::Error> {
        let mock_proof = Proof::new(ProofKind::Compressed, b"mock_proof".to_vec());
        Ok((
            b"mock_public_input".to_vec(),
            mock_proof,
            ProgramProvingReport {
                proving_time: Duration::from_millis(1),
            },
        ))
    }

    fn verify(&self, proof: &Proof) -> Result<PublicValues, anyhow::Error> {
        let mock_proof = Proof::new(ProofKind::Compressed, b"mock_proof".to_vec());
        let public_values = b"mock_public_input".to_vec();
        
        if *proof.as_bytes() == *mock_proof.as_bytes() {
            Ok(public_values)
        } else {
            Err(anyhow::anyhow!("invalid proof"))
        }
    }
    
    fn name(&self) -> &'static str {
        "mock_zkvm"
    }

    fn sdk_version(&self) -> &'static str {
        "0.0.0"
    }
}

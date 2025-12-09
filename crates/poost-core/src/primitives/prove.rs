use ere_zkvm_interface::{Proof, ProofKind};
use serde::{Deserialize, Serialize};
use crate::{common::ProgramID, program::ProgramInput};


#[derive(Debug, Serialize, Deserialize)]
pub struct ProveRequest {
    pub program_id: ProgramID,
    pub input: ProgramInput,
    pub proof_kind: ProveRequestKind,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProveResponse {
    pub program_id: ProgramID,
    pub proof: Proof,
    pub proving_time_milliseconds: u128,
    pub public_inputs: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ProveRequestKind {
    Compressed,
    Groth16,
}

impl From<ProveRequestKind> for ProofKind {
    fn from(kind: ProveRequestKind) -> Self {
        match kind {
            ProveRequestKind::Compressed => ProofKind::Compressed,
            ProveRequestKind::Groth16 => ProofKind::Groth16,
        }
    }
}

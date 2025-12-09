use serde::{Deserialize, Serialize};
use crate::{common::ProgramID, program::ProgramInput};


#[derive(Debug, Serialize, Deserialize)]
pub struct ProveRequest {
    pub program_id: ProgramID,
    pub input: ProgramInput,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProveResponse {
    pub program_id: ProgramID,
    pub proof: Vec<u8>,
    pub proving_time_milliseconds: u128,
}
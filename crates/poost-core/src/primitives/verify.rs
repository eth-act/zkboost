use serde::{Deserialize, Serialize};
use crate::common::ProgramID;


#[derive(Debug, Serialize, Deserialize)]
pub struct VerifyRequest {
    pub program_id: ProgramID,
    pub proof: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VerifyResponse {
    pub program_id: ProgramID,
    pub verified: bool,
    // Empty if verification returned true
    pub failure_reason: String,
}
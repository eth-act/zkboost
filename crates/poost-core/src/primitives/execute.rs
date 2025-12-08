use std::time::Duration;
use serde::{Deserialize, Serialize};

use crate::common::ProgramID;

// #[derive(Debug, Serialize, Deserialize)]
// pub struct ExecuteRequest {
//     pub program_id: ProgramID,
//     pub input: ProgramInput,
// }

// #[derive(Debug, Serialize, Deserialize)]
// pub struct ExecuteResponse {
//     pub program_id: ProgramID,
//     pub total_num_cycles: u64,
//     pub region_cycles: IndexMap<String, u64>,
//     pub execution_time_duration: Duration,
// }
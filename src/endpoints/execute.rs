use std::time::Duration;

use axum::{Json, extract::State, http::StatusCode};
use ere_zkvm_interface::{Input, PublicValues, zkVM};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_with::{base64::Base64, serde_as};
use tracing::instrument;

use crate::common::{AppState, ProgramID};

#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub program_id: ProgramID,
    #[serde_as(as = "Base64")]
    pub input: Vec<u8>,
}

#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecuteResponse {
    pub program_id: ProgramID,
    #[serde_as(as = "Base64")]
    pub public_values: PublicValues,
    pub total_num_cycles: u64,
    pub region_cycles: IndexMap<String, u64>,
    pub execution_duration: Duration,
}

#[axum::debug_handler]
#[instrument(skip_all)]
pub async fn execute_program(
    State(state): State<AppState>,
    Json(req): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, (StatusCode, String)> {
    let program_id = req.program_id.clone();
    let programs = state.programs.read().await;

    let program = programs
        .get(&program_id)
        .ok_or((StatusCode::NOT_FOUND, "Program not found".to_string()))?;

    let input = Input::new().with_stdin(req.input);

    let (public_values, report) = program.vm.execute(&input).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to execute program: {}", e),
        )
    })?;

    Ok(Json(ExecuteResponse {
        program_id,
        public_values,
        total_num_cycles: report.total_num_cycles,
        region_cycles: report.region_cycles,
        execution_duration: report.execution_duration,
    }))
}

#[cfg(test)]
mod tests {
    use axum::{Json, extract::State, http::StatusCode};

    use crate::{
        common::{AppState, ProgramID},
        endpoints::{execute::ExecuteRequest, execute_program},
        mock_zkvm::mock_app_state,
    };

    #[tokio::test]
    async fn test_execute_success() {
        let program_id = ProgramID("mock_program_id".to_string());
        let state = mock_app_state(&program_id);

        let request = ExecuteRequest {
            program_id: program_id.clone(),
            input: Vec::new(),
        };

        let response = execute_program(State(state), Json(request)).await.unwrap();

        assert_eq!(response.program_id, program_id);
    }

    #[tokio::test]
    async fn test_execute_program_not_found() {
        let state = AppState::default();

        let request = ExecuteRequest {
            program_id: ProgramID("non_existent".to_string()),
            input: Vec::new(),
        };

        let result = execute_program(State(state), Json(request)).await;

        assert!(result.is_err());
        let (status, message) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(message, "Program not found");
    }
}

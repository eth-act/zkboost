use axum::{Json, extract::State, http::StatusCode};
use ere_zkvm_interface::{Input, zkVM};
use tracing::instrument;
use zkboost_types::{ExecuteRequest, ExecuteResponse};

use crate::app::AppState;

/// HTTP handler for the `/execute` endpoint.
///
/// Executes a zkVM program without generating a proof.
#[axum::debug_handler]
#[instrument(skip_all)]
pub(crate) async fn execute_program(
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
            format!("Failed to execute program: {e}"),
        )
    })?;

    Ok(Json(ExecuteResponse {
        program_id,
        public_values,
        total_num_cycles: report.total_num_cycles,
        region_cycles: report.region_cycles,
        execution_time_ms: report.execution_duration.as_millis(),
    }))
}

#[cfg(test)]
mod tests {
    use axum::{Json, extract::State, http::StatusCode};
    use zkboost_types::{ExecuteRequest, ProgramID};

    use crate::{
        app::{AppState, execute::execute_program},
        mock::mock_app_state,
    };

    #[tokio::test]
    async fn test_execute_success() {
        let program_id = ProgramID::from("mock_program_id");
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
            program_id: ProgramID::from("non_existent"),
            input: Vec::new(),
        };

        let result = execute_program(State(state), Json(request)).await;

        assert!(result.is_err());
        let (status, message) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(message, "Program not found");
    }
}

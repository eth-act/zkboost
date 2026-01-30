use std::time::Instant;

use axum::{Json, extract::State, http::StatusCode};
use ere_zkvm_interface::Input;
use tracing::instrument;
use zkboost_types::{ExecuteRequest, ExecuteResponse};

use crate::{app::AppState, metrics::record_execute};

/// HTTP handler for the `/execute` endpoint.
///
/// Executes a zkVM program without generating a proof.
#[instrument(skip_all)]
pub(crate) async fn execute_program(
    State(state): State<AppState>,
    Json(req): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, (StatusCode, String)> {
    let start = Instant::now();
    let program_id = req.program_id.clone();

    let zkvm = state.programs.get(&program_id).ok_or_else(|| {
        record_execute(&program_id.0, false, start.elapsed(), 0);
        (StatusCode::NOT_FOUND, "Program not found".to_string())
    })?;

    let input = Input::new().with_stdin(req.input);

    let (public_values, report) = zkvm.execute(input).await.map_err(|e| {
        record_execute(&program_id.0, false, start.elapsed(), 0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to execute program: {e}"),
        )
    })?;

    record_execute(
        &program_id.0,
        true,
        start.elapsed(),
        report.total_num_cycles,
    );

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

    use crate::{app::execute::execute_program, mock::tests::mock_app_state};

    #[tokio::test]
    async fn test_execute_success() {
        let program_id = ProgramID::from("mock_program_id");
        let state = mock_app_state(Some(&program_id));

        let request = ExecuteRequest {
            program_id: program_id.clone(),
            input: Vec::new(),
        };

        let response = execute_program(State(state), Json(request)).await.unwrap();

        assert_eq!(response.program_id, program_id);
    }

    #[tokio::test]
    async fn test_execute_program_not_found() {
        let state = mock_app_state(None);

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

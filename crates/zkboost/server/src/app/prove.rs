use std::time::Instant;

use axum::{Json, extract::State, http::StatusCode};
use ere_zkvm_interface::{Input, Proof, ProofKind};
use tracing::instrument;
use zkboost_types::{ProveRequest, ProveResponse};

use crate::{app::AppState, metrics::record_prove};

/// HTTP handler for the `/prove` endpoint.
///
/// Executes a zkVM program and generates a cryptographic proof.
#[instrument(skip_all)]
pub(crate) async fn prove_program(
    State(state): State<AppState>,
    Json(req): Json<ProveRequest>,
) -> Result<Json<ProveResponse>, (StatusCode, String)> {
    let start = Instant::now();
    let program_id = req.program_id.clone();
    let programs = state.programs.read().await;

    let program = programs.get(&program_id).ok_or_else(|| {
        record_prove(&program_id.0, false, start.elapsed(), 0);
        (StatusCode::NOT_FOUND, "Program not found".to_string())
    })?;

    let input = Input::new().with_stdin(req.input);

    let (public_values, proof, report) =
        program
            .vm
            .prove(&input, ProofKind::Compressed)
            .map_err(|e| {
                record_prove(&program_id.0, false, start.elapsed(), 0);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to generate proof: {e}"),
                )
            })?;

    let Proof::Compressed(proof) = proof else {
        record_prove(&program_id.0, false, start.elapsed(), 0);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected proof kind: {:?}", proof.kind()),
        ));
    };

    record_prove(&program_id.0, true, start.elapsed(), proof.len());

    Ok(Json(ProveResponse {
        program_id,
        public_values,
        proof,
        proving_time_ms: report.proving_time.as_millis(),
    }))
}

#[cfg(test)]
mod tests {
    use axum::{Json, extract::State, http::StatusCode};
    use zkboost_types::{ProgramID, ProveRequest};

    use crate::{app::prove::prove_program, mock::mock_app_state};

    #[tokio::test]
    async fn test_prove_success() {
        let program_id = ProgramID::from("mock_program_id");
        let state = mock_app_state(Some(&program_id));

        let request = ProveRequest {
            program_id: program_id.clone(),
            input: Vec::new(),
        };

        let response = prove_program(State(state), Json(request)).await.unwrap();

        assert_eq!(response.program_id, program_id);
        assert!(!response.proof.is_empty()); // Check that the proof is not empty
    }

    #[tokio::test]
    async fn test_prove_program_not_found() {
        let state = mock_app_state(None);

        let request = ProveRequest {
            program_id: ProgramID::from("non_existent"),
            input: Vec::new(),
        };

        let result = prove_program(State(state), Json(request)).await;

        assert!(result.is_err());
        let (status, message) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(message, "Program not found");
    }
}

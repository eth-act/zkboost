use axum::{Json, extract::State, http::StatusCode};
use ere_zkvm_interface::{Input, Proof, ProofKind, PublicValues};
use serde::{Deserialize, Serialize};
use serde_with::{base64::Base64, serde_as};
use tracing::instrument;

use crate::common::{AppState, ProgramID};

#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct ProveRequest {
    pub program_id: ProgramID,
    #[serde_as(as = "Base64")]
    pub input: Vec<u8>,
}

#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct ProveResponse {
    pub program_id: ProgramID,
    #[serde_as(as = "Base64")]
    pub public_values: PublicValues,
    #[serde_as(as = "Base64")]
    pub proof: Vec<u8>,
    pub proving_time_milliseconds: u128,
}

#[axum::debug_handler]
#[instrument(skip_all)]
pub async fn prove_program(
    State(state): State<AppState>,
    Json(req): Json<ProveRequest>,
) -> Result<Json<ProveResponse>, (StatusCode, String)> {
    let program_id = req.program_id.clone();
    let programs = state.programs.read().await;

    let program = programs
        .get(&program_id)
        .ok_or((StatusCode::NOT_FOUND, "Program not found".to_string()))?;

    let input = Input::new().with_stdin(req.input);

    let (public_values, proof, report) =
        program
            .vm
            .prove(&input, ProofKind::Compressed)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to generate proof: {}", e),
                )
            })?;

    let Proof::Compressed(proof) = proof else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unexpected proof kind: {:?}", proof.kind()),
        ));
    };

    Ok(Json(ProveResponse {
        program_id,
        public_values,
        proof,
        proving_time_milliseconds: report.proving_time.as_millis(),
    }))
}

#[cfg(test)]
mod tests {
    use axum::{Json, extract::State, http::StatusCode};

    use crate::{
        common::{AppState, ProgramID},
        endpoints::{prove::ProveRequest, prove_program},
        mock_zkvm::mock_app_state,
    };

    #[tokio::test]
    async fn test_prove_success() {
        let program_id = ProgramID("mock_program_id".to_string());
        let state = mock_app_state(&program_id);

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
        let state = AppState::default();

        let request = ProveRequest {
            program_id: ProgramID("non_existent".to_string()),
            input: Vec::new(),
        };

        let result = prove_program(State(state), Json(request)).await;

        assert!(result.is_err());
        let (status, message) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(message, "Program not found");
    }
}

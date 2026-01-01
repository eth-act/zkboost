use axum::{Json, extract::State, http::StatusCode};
use ere_zkvm_interface::Proof;
use tracing::instrument;
use zkboost_types::{VerifyRequest, VerifyResponse};

use crate::app::AppState;

/// HTTP handler for the `/verify` endpoint.
///
/// Verifies a cryptographic proof without re-executing the program.
#[instrument(skip_all)]
pub(crate) async fn verify_proof(
    State(state): State<AppState>,
    Json(req): Json<VerifyRequest>,
) -> Result<Json<VerifyResponse>, (StatusCode, String)> {
    // Check if the program_id is correct
    let programs = state.programs.read().await;

    let program = programs
        .get(&req.program_id)
        .ok_or((StatusCode::NOT_FOUND, "Program not found".to_string()))?;

    // Verify the proof
    let (verified, public_values, failure_reason) =
        match program.vm.verify(&Proof::Compressed(req.proof)) {
            Ok(public_values) => (true, public_values, String::new()),
            Err(err) => {
                let failure_reason = match err.downcast_ref::<ere_dockerized::zkvm::Error>() {
                    Some(ere_dockerized::zkvm::Error::zkVM(err)) => err.to_string(),
                    // Connection or RPC errors
                    Some(err) => return Err((StatusCode::INTERNAL_SERVER_ERROR, err.to_string())),
                    None => err.to_string(),
                };
                (false, Vec::new(), failure_reason)
            }
        };

    Ok(Json(VerifyResponse {
        program_id: req.program_id,
        verified,
        public_values,
        failure_reason,
    }))
}

#[cfg(test)]
mod tests {
    use axum::{Json, extract::State, http::StatusCode};
    use zkboost_types::{ProgramID, ProveRequest, VerifyRequest};

    use crate::{
        app::{AppState, prove::prove_program, verify::verify_proof},
        mock::mock_app_state,
    };

    #[tokio::test]
    async fn test_verify_valid_proof() {
        let program_id = ProgramID::from("mock_program_id");
        let state = mock_app_state(&program_id);

        let request = ProveRequest {
            program_id: program_id.clone(),
            input: Vec::new(),
        };

        let result = prove_program(State(state.clone()), Json(request))
            .await
            .unwrap();

        // Create a request
        let request = VerifyRequest {
            program_id: result.program_id.clone(),
            proof: result.proof.clone(),
        };

        // Call the handler
        let response = verify_proof(State(state), Json(request)).await.unwrap();

        // Verify the response
        assert_eq!(response.program_id, program_id);
        assert!(response.verified);
    }

    #[tokio::test]
    async fn test_verify_invalid_proof() {
        let program_id = ProgramID::from("mock_program_id");
        let state = mock_app_state(&program_id);

        let request = VerifyRequest {
            program_id: program_id.clone(),
            proof: b"invalid_proof".to_vec(),
        };

        let result = verify_proof(State(state), Json(request)).await;
        // The endpoint returns a result if the verification fails.
        // We need to check the proof response to know whether it failed
        // verification and for what reason.
        assert!(result.is_ok());
        assert!(!result.unwrap().verified);
    }

    #[tokio::test]
    async fn test_verify_program_not_found() {
        let state = AppState::default();

        let request = VerifyRequest {
            program_id: ProgramID::from("non_existent"),
            proof: b"example_proof".to_vec(),
        };

        let result = verify_proof(State(state), Json(request)).await;

        assert!(result.is_err());
        let (status, message) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(message, "Program not found");
    }
}

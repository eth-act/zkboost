use std::time::Instant;

use axum::{Json, extract::State, http::StatusCode};
use ere_zkvm_interface::Proof;
use tracing::instrument;
use zkboost_types::{VerifyRequest, VerifyResponse};

use crate::{app::AppState, metrics::record_verify};

/// HTTP handler for the `/verify` endpoint.
///
/// Verifies a cryptographic proof without re-executing the program.
#[instrument(skip_all)]
pub(crate) async fn verify_proof(
    State(state): State<AppState>,
    Json(req): Json<VerifyRequest>,
) -> Result<Json<VerifyResponse>, (StatusCode, String)> {
    let start = Instant::now();
    let program_id = req.program_id.clone();

    // Check if the program_id is correct

    let zkvm = state.programs.get(&program_id).ok_or_else(|| {
        // Record as failed verification for program not found
        record_verify(&program_id.0, false, start.elapsed());
        (StatusCode::NOT_FOUND, "Program not found".to_string())
    })?;

    // Verify the proof
    let (verified, public_values, failure_reason) =
        match zkvm.verify(Proof::Compressed(req.proof)).await {
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

    record_verify(&program_id.0, verified, start.elapsed());

    Ok(Json(VerifyResponse {
        program_id,
        verified,
        public_values,
        failure_reason,
    }))
}

#[cfg(test)]
mod tests {
    use axum::{Json, extract::State, http::StatusCode};
    use zkboost_types::{ProgramID, VerifyRequest};

    use crate::{app::verify::verify_proof, mock::tests::mock_app_state};

    #[tokio::test]
    async fn test_verify_valid_proof() {
        let program_id = ProgramID::from("mock_program_id");
        let state = mock_app_state(Some(&program_id));

        // Create a request with the mock proof that MockzkVM accepts
        let request = VerifyRequest {
            program_id: program_id.clone(),
            proof: b"mock_proof".to_vec(),
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
        let state = mock_app_state(Some(&program_id));

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
        let state = mock_app_state(None);

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

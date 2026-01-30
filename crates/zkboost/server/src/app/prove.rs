use axum::{Json, extract::State, http::StatusCode};
use ere_zkvm_interface::Input;
use tracing::instrument;
use uuid::Uuid;
use zkboost_types::{ProofGenId, ProveRequest, ProveResponse};

use crate::{app::AppState, proof_service::ProofMessage};

/// HTTP handler for the `/prove` endpoint.
///
/// Queues a proof request and returns immediately with a `ProofGenId`.
#[instrument(skip_all)]
pub(crate) async fn prove_program(
    State(state): State<AppState>,
    Json(req): Json<ProveRequest>,
) -> Result<Json<ProveResponse>, (StatusCode, String)> {
    let program_id = req.program_id.clone();

    let proof_tx = state
        .proof_txs
        .get(&program_id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Program not found".to_string()))?;

    let proof_gen_id = ProofGenId(Uuid::new_v4().to_string());

    let msg = ProofMessage {
        proof_gen_id: proof_gen_id.clone(),
        input: Input::new().with_stdin(req.input),
    };
    proof_tx.send(msg).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to send proof: {e}"),
        )
    })?;

    Ok(Json(ProveResponse { proof_gen_id }))
}

#[cfg(test)]
mod tests {
    use axum::{Json, extract::State, http::StatusCode};
    use zkboost_types::{ProgramID, ProveRequest};

    use crate::{app::prove::prove_program, mock::tests::mock_app_state};

    #[tokio::test]
    async fn test_prove_success() {
        let program_id = ProgramID::from("mock_program_id");
        let state = mock_app_state(Some(&program_id));

        let request = ProveRequest {
            program_id: program_id.clone(),
            input: Vec::new(),
        };

        let response = prove_program(State(state), Json(request)).await.unwrap();

        // Verify that proof_id is a valid UUID
        assert!(uuid::Uuid::parse_str(&response.proof_gen_id.0).is_ok());
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

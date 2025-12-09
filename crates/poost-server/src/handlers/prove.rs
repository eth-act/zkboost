use axum::{Json, extract::State, http::StatusCode};
use ere_zkvm_interface::{Input, ProofKind};
use poost_core::primitives::prove::{ProveRequest, ProveResponse};
use tracing::instrument;
use crate::app_state::AppState;



#[instrument(skip_all)]
pub async fn prove_program(
    State(state): State<AppState>,
    Json(req): Json<ProveRequest>,
) -> Result<Json<ProveResponse>, (StatusCode, String)> {
    let program_id = req.program_id.clone();
    let programs = state.programs.read().await;
    let proof_kind: ProofKind = req.proof_kind.into();

    let program = programs
        .get(&program_id)
        .ok_or((StatusCode::NOT_FOUND, "Program not found".to_string()))?;

    let input = Input::new().with_prefixed_stdin(req.input.input);

    let (public_inputs, proof, report) = program.zkvm_instance.vm.prove(&input, proof_kind).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to execute program: {}", e),
        )
    })?;

    Ok(Json(ProveResponse {
        program_id,
        proof,
        proving_time_milliseconds: report.proving_time.as_millis(),
        public_inputs
    }))
}
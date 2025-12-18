//! zkVM input generation for Ethereum Execution Layer stateless validation.

use benchmark_runner::stateless_validator::{ethrex, reth};
use reth_stateless::StatelessInput;
use witness_generator::StatelessValidationFixture;
use zkboost_ethereum_el_types::ElKind;

/// Generates zkVM input for given EL from stateless input.
///
/// # Arguments
///
/// * `el` - The execution layer kind (Reth or Ethrex)
/// * `input` - The stateless input containing block and witness data
///
/// # Returns
///
/// Serialized bytes suitable for the specified zkVM guest program.
pub fn generate_input(el: ElKind, input: &StatelessInput) -> Result<Vec<u8>, anyhow::Error> {
    let fixture = StatelessValidationFixture::from_stateless_input(input, el.as_str());
    let fixtures = [fixture];

    let guest_input = match el {
        ElKind::Reth => reth::stateless_validator_inputs_from_fixture(&fixtures)?,
        ElKind::Ethrex => ethrex::stateless_validator_inputs_from_fixture(&fixtures)?,
    };

    let input_data = guest_input
        .into_iter()
        .next()
        .expect("should have exactly one input")
        .input()?;

    Ok(input_data.stdin)
}

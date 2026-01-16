//! zkVM input generation for Ethereum Execution Layer stateless validation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use ere_zkvm_interface::Input;
use sha2::{Digest, Sha256};
use stateless_validator_ethrex::guest::{
    Io, StatelessValidatorEthrexInput, StatelessValidatorEthrexIo,
};
use stateless_validator_reth::guest::{
    StatelessValidatorOutput, StatelessValidatorRethInput, StatelessValidatorRethIo,
};
use zkboost_ethereum_el_types::ElKind;

#[rustfmt::skip]
pub use reth_stateless::StatelessInput;

/// Necessary input for EL guest programs
#[derive(Debug)]
pub struct ElInput {
    stateless_input: StatelessInput,
}

impl ElInput {
    /// Constructs a new `ElInput`
    pub fn new(stateless_input: StatelessInput) -> Self {
        Self { stateless_input }
    }

    /// Generates zkVM input for given EL from stateless input.
    ///
    /// # Arguments
    ///
    /// * `el` - The execution layer kind (Reth or Ethrex)
    ///
    /// # Returns
    ///
    /// [`Input`] for the zkVM methods of the specified EL guest program.
    pub fn to_zkvm_input(&self, el: ElKind) -> anyhow::Result<Input> {
        let stdin = match el {
            ElKind::Ethrex => StatelessValidatorEthrexIo::serialize_input(
                &StatelessValidatorEthrexInput::new(&self.stateless_input)?,
            )?,
            ElKind::Reth => StatelessValidatorRethIo::serialize_input(
                &StatelessValidatorRethInput::new(&self.stateless_input)?,
            )?,
        };
        Ok(Input::new().with_prefixed_stdin(stdin))
    }

    /// Returns expected sha256 hash of output given the EL and whether the
    /// stateless validation is successful or not.
    pub fn expected_public_values(&self, el: ElKind, success: bool) -> anyhow::Result<[u8; 32]> {
        let output = StatelessValidatorOutput::new(
            self.stateless_input.block.hash_slow().0,
            self.stateless_input.block.parent_hash.0,
            success,
        );
        let serialized_output = match el {
            ElKind::Ethrex => StatelessValidatorEthrexIo::serialize_output(&output)?,
            ElKind::Reth => StatelessValidatorRethIo::serialize_output(&output)?,
        };
        Ok(Sha256::digest(serialized_output).into())
    }
}

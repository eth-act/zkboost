//! zkVM input generation for Ethereum Execution Layer stateless validation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use reth_stateless::StatelessInput;
use zkboost_ethereum_el_types::ElKind;

/// Generates zkVM input for given EL from stateless input.
pub fn generate_input(_el: ElKind, _input: &StatelessInput) -> Vec<u8> {
    todo!()
}

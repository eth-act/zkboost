//! Shared types for the zkboost proof node and the mock attestor.
//!
//! This crate contains the proof type identifiers, the conversion between the Engine API
//! `engine_newPayloadV4` and `engine_newPayloadV5` parameters and the stateless validator
//! `NewPayloadRequest`, and the EIP-8025 execution proof envelopes.

#![warn(unused_crate_dependencies)]

pub use execution_proof::*;
pub use new_payload::NewPayloadParams;
pub use proof_type::*;
pub use stateless_validator_common::{
    HashTreeRoot, ProgressiveList, Sha2Hasher, SszDecode, SszEncode, SszList, SszVector,
    guest::{
        StatelessInput, StatelessValidationResult,
        input::{ExecutionWitness, ProtocolFork, PublicKeys, new_payload_request::*},
    },
};

mod execution_proof;
mod new_payload;
mod proof_type;

/// 32-byte hash.
pub type Hash256 = alloy_primitives::B256;

/// Proof bytes of the mock zkVM, accepted as valid without a verifier.
pub const MOCK_PROOF: &[u8] = b"MOCK";

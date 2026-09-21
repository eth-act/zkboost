//! Shared types for the zkboost proof node and the mock beacon node. The crate holds the proof type
//! identifiers, the `engine_newPayloadV5` conversion, and the EIP-8025 execution proof envelopes.

#![warn(unused_crate_dependencies)]

pub use execution_proof::*;
pub use new_payload::{NewPayloadParams, NewPayloadRequestExt};
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

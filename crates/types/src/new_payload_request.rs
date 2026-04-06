//! `NewPayloadRequest` with SSZ `Encode/Decode` and `TreeHash` derived.
//!
//! Copied from [`execution_layer::NewPayloadRequest`].
//!
//! [`execution_layer::NewPayloadRequest`]: https://github.com/sigp/lighthouse/blob/unstable/beacon_node/execution_layer/src/engine_api/new_payload_request.rs

#![allow(missing_docs)]

use lighthouse_types::{
    BeaconStateError, EthSpec, ExecutionPayloadBellatrix, ExecutionPayloadCapella,
    ExecutionPayloadDeneb, ExecutionPayloadElectra, ExecutionPayloadFulu, ExecutionRequests,
    Hash256, VersionedHash,
};
use ssz_derive::{Decode, Encode};
use ssz_types::VariableList;
use superstruct::superstruct;
use tree_hash_derive::TreeHash;

#[superstruct(
    variants(Bellatrix, Capella, Deneb, Electra, Fulu, Gloas),
    variant_attributes(derive(Clone, Debug, PartialEq, Encode, Decode, TreeHash)),
    map_into(ExecutionPayload),
    map_ref_into(ExecutionPayloadRef),
    cast_error(
        ty = "BeaconStateError",
        expr = "BeaconStateError::IncorrectStateVariant"
    ),
    partial_getter_error(
        ty = "BeaconStateError",
        expr = "BeaconStateError::IncorrectStateVariant"
    )
)]
#[derive(Clone, Debug, PartialEq, Encode, Decode, TreeHash)]
#[ssz(enum_behaviour = "transparent")]
#[tree_hash(enum_behaviour = "transparent")]
pub struct NewPayloadRequest<E: EthSpec> {
    #[superstruct(
        only(Bellatrix),
        partial_getter(rename = "execution_payload_bellatrix")
    )]
    pub execution_payload: ExecutionPayloadBellatrix<E>,
    #[superstruct(only(Capella), partial_getter(rename = "execution_payload_capella"))]
    pub execution_payload: ExecutionPayloadCapella<E>,
    #[superstruct(only(Deneb), partial_getter(rename = "execution_payload_deneb"))]
    pub execution_payload: ExecutionPayloadDeneb<E>,
    #[superstruct(only(Electra), partial_getter(rename = "execution_payload_electra"))]
    pub execution_payload: ExecutionPayloadElectra<E>,
    #[superstruct(only(Fulu), partial_getter(rename = "execution_payload_fulu"))]
    pub execution_payload: ExecutionPayloadFulu<E>,
    #[superstruct(only(Gloas), partial_getter(rename = "execution_payload_gloas"))]
    pub execution_payload: lighthouse_types::ExecutionPayloadGloas<E>,
    #[superstruct(only(Deneb, Electra, Fulu, Gloas))]
    pub versioned_hashes: VariableList<VersionedHash, E::MaxBlobCommitmentsPerBlock>,
    #[superstruct(only(Deneb, Electra, Fulu, Gloas))]
    pub parent_beacon_block_root: Hash256,
    #[superstruct(only(Electra, Fulu, Gloas))]
    pub execution_requests: ExecutionRequests<E>,
}

impl<E: EthSpec> NewPayloadRequest<E> {
    /// Returns the block hash from the execution payload.
    pub fn block_hash(&self) -> Hash256 {
        match self {
            Self::Bellatrix(inner) => inner.execution_payload.block_hash.0,
            Self::Capella(inner) => inner.execution_payload.block_hash.0,
            Self::Deneb(inner) => inner.execution_payload.block_hash.0,
            Self::Electra(inner) => inner.execution_payload.block_hash.0,
            Self::Fulu(inner) => inner.execution_payload.block_hash.0,
            Self::Gloas(inner) => inner.execution_payload.block_hash.0,
        }
    }

    /// Returns the block number from the execution payload.
    pub fn block_number(&self) -> u64 {
        match self {
            Self::Bellatrix(inner) => inner.execution_payload.block_number,
            Self::Capella(inner) => inner.execution_payload.block_number,
            Self::Deneb(inner) => inner.execution_payload.block_number,
            Self::Electra(inner) => inner.execution_payload.block_number,
            Self::Fulu(inner) => inner.execution_payload.block_number,
            Self::Gloas(inner) => inner.execution_payload.block_number,
        }
    }

    /// Returns the timestamp from the execution payload.
    pub fn timestamp(&self) -> u64 {
        match self {
            Self::Bellatrix(inner) => inner.execution_payload.timestamp,
            Self::Capella(inner) => inner.execution_payload.timestamp,
            Self::Deneb(inner) => inner.execution_payload.timestamp,
            Self::Electra(inner) => inner.execution_payload.timestamp,
            Self::Fulu(inner) => inner.execution_payload.timestamp,
            Self::Gloas(inner) => inner.execution_payload.timestamp,
        }
    }

    /// Returns the gas used from the execution payload.
    pub fn gas_used(&self) -> u64 {
        match self {
            Self::Bellatrix(inner) => inner.execution_payload.gas_used,
            Self::Capella(inner) => inner.execution_payload.gas_used,
            Self::Deneb(inner) => inner.execution_payload.gas_used,
            Self::Electra(inner) => inner.execution_payload.gas_used,
            Self::Fulu(inner) => inner.execution_payload.gas_used,
            Self::Gloas(inner) => inner.execution_payload.gas_used,
        }
    }
}

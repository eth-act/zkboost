//! Assembles the `StatelessInput` from a `NewPayloadRequest`, its execution witness, and the
//! chain id, then encodes it once to schema-id-prefixed SSZ bytes consumed by every zkVM backend.

use alloy_consensus::{EthereumTxEnvelope, TxEip4844};
use alloy_eips::Decodable2718;
use anyhow::Context;
use stateless_validator_common::guest::input::PUBLIC_KEY_BYTES;
use zkboost_types::{
    ExecutionWitness, Hash256, HashTreeRoot, NewPayloadRequest, ProtocolFork, PublicKeys,
    Sha2Hasher, Transactions,
};

/// A wrapper for `stateless_input_bytes` with payload metadata.
#[derive(Debug)]
pub(crate) struct StatelessInput {
    stateless_input_bytes: Vec<u8>,
    new_payload_request_root: Hash256,
    block_hash: Hash256,
    parent_beacon_block_root: Hash256,
    block_number: u64,
    gas_used: u64,
}

impl StatelessInput {
    /// Builds the `StatelessInput` of a Gloas payload request, proven under the Amsterdam rules.
    pub(crate) fn new(
        new_payload_request: NewPayloadRequest,
        witness: ExecutionWitness,
        chain_id: u64,
    ) -> anyhow::Result<Self> {
        let NewPayloadRequest::Gloas(request) = &new_payload_request else {
            unreachable!("engine_newPayloadV5 params convert to a gloas request")
        };
        let payload = &request.execution_payload;
        let block_hash = Hash256::from(payload.block_hash);
        let parent_beacon_block_root = Hash256::from(request.parent_beacon_block_root);
        let block_number = payload.block_number;
        let gas_used = payload.gas_used;
        let public_keys = PublicKeys::from(recover_public_keys(&payload.transactions)?);
        // The root follows the request layout of ere-guests, which the guests commit. lighthouse
        // hashes the request as a progressive container with a bounded list of versioned hashes.
        let new_payload_request_root =
            Hash256::from(new_payload_request.hash_tree_root(&Sha2Hasher));

        let stateless_input_bytes = stateless_validator_common::guest::StatelessInput {
            new_payload_request,
            witness,
            chain_id,
            public_keys,
        }
        .to_schema_prefixed_ssz(ProtocolFork::Amsterdam);

        Ok(Self {
            new_payload_request_root,
            stateless_input_bytes,
            block_hash,
            parent_beacon_block_root,
            block_number,
            gas_used,
        })
    }

    /// Returns the hash-tree-root of the `NewPayloadRequest`.
    pub(crate) fn root(&self) -> Hash256 {
        self.new_payload_request_root
    }

    /// Returns the schema-id-prefixed SSZ bytes used as zkVM stdin.
    pub(crate) fn stateless_input_bytes(&self) -> &[u8] {
        &self.stateless_input_bytes
    }

    /// Returns the block hash.
    pub(crate) fn block_hash(&self) -> Hash256 {
        self.block_hash
    }

    /// Returns the parent beacon block root of the payload.
    pub(crate) fn parent_beacon_block_root(&self) -> Hash256 {
        self.parent_beacon_block_root
    }

    /// Returns the block number.
    pub(crate) fn block_number(&self) -> u64 {
        self.block_number
    }

    /// Returns the gas used by the block, for mock proving-time simulation.
    pub(crate) fn gas_used(&self) -> u64 {
        self.gas_used
    }
}

/// Recovers public keys from transaction signatures in the payload.
fn recover_public_keys(transactions: &Transactions) -> anyhow::Result<Vec<[u8; PUBLIC_KEY_BYTES]>> {
    transactions
        .iter()
        .enumerate()
        .map(|(index, transaction)| {
            let transaction =
                EthereumTxEnvelope::<TxEip4844>::decode_2718(&mut transaction.as_ref())
                    .with_context(|| format!("failed to decode tx #{index}"))?;
            transaction
                .signature()
                .recover_from_prehash(&transaction.signature_hash())
                .map(|key| key.to_encoded_point(false).as_bytes().try_into().unwrap())
                .with_context(|| format!("failed to recover signature for tx #{index}"))
        })
        .collect()
}

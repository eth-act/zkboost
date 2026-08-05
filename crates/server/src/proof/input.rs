//! Assembles the `StatelessInput` from a `NewPayloadRequest`, its execution witness, and
//! chain config, then encodes it once to schema-id-prefixed SSZ bytes consumed by every
//! zkVM backend.

use alloy_consensus::{EthereumTxEnvelope, TxEip4844};
use alloy_eips::Decodable2718;
use anyhow::Context;
use stateless_validator_common::guest::input::PUBLIC_KEY_BYTES;
use zkboost_types::{ChainConfig, ExecutionWitness, Hash256, NewPayloadRequest, ProtocolFork};

/// A wrapper for `stateless_input_bytes` with payload metadata.
#[derive(Debug)]
pub(crate) struct StatelessInput {
    stateless_input_bytes: Vec<u8>,
    new_payload_request_root: Hash256,
    block_hash: Hash256,
    block_number: u64,
    gas_used: u64,
}

impl StatelessInput {
    /// Builds the `StatelessInput`.
    pub(crate) fn new(
        fork: ProtocolFork,
        new_payload_request: &NewPayloadRequest,
        new_payload_request_root: Hash256,
        witness: &ExecutionWitness,
        chain_config: &ChainConfig,
    ) -> anyhow::Result<Self> {
        let block_hash = Hash256::from(new_payload_request.block_hash());
        let block_number = new_payload_request.block_number();
        let gas_used = new_payload_request.gas_used();

        let stateless_input_bytes = stateless_validator_common::guest::StatelessInput {
            new_payload_request: new_payload_request.clone(),
            witness: witness.clone(),
            chain_config: chain_config.clone(),
            public_keys: recover_public_keys(new_payload_request)?.try_into()?,
        }
        .to_schema_prefixed_ssz(fork);

        Ok(Self {
            new_payload_request_root,
            stateless_input_bytes,
            block_hash,
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
fn recover_public_keys(
    new_payload_request: &NewPayloadRequest,
) -> anyhow::Result<Vec<[u8; PUBLIC_KEY_BYTES]>> {
    new_payload_request
        .transactions()
        .into_iter()
        .enumerate()
        .map(|(i, tx)| {
            let tx = EthereumTxEnvelope::<TxEip4844>::decode_2718(&mut tx.as_ref())
                .with_context(|| format!("failed to decode tx #{i}"))?;
            tx.signature()
                .recover_from_prehash(&tx.signature_hash())
                .map(|key| key.to_encoded_point(false).as_bytes().try_into().unwrap())
                .with_context(|| format!("failed to recover signature for tx #{i}"))
        })
        .collect()
}

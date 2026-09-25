//! Assembles the `StatelessInput` of a payload and encodes it once to schema-id-prefixed SSZ.

use std::time::Instant;

use alloy_consensus::{EthereumTxEnvelope, TxEip4844};
use alloy_eips::Decodable2718;
use anyhow::Context;
use stateless_validator_common::guest::input::PUBLIC_KEY_BYTES;
use zkboost_types::{
    ExecutionWitness, Hash256, HashTreeRoot, NewPayloadRequest, NewPayloadRequestExt, ProtocolFork,
    PublicKeys, Sha2Hasher, Transactions,
};

/// The metadata of a `NewPayloadRequest`, known before the witness. The root identifies the
/// request and its proofs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewPayloadRequestMeta {
    /// The hash tree root of the `NewPayloadRequest`, as the guests commit it.
    pub(crate) new_payload_request_root: Hash256,
    /// Block hash of the payload.
    pub(crate) block_hash: Hash256,
    /// Parent beacon block root of the payload.
    pub(crate) parent_beacon_block_root: Hash256,
    /// Block number of the payload.
    pub(crate) block_number: u64,
    /// Slot of the payload.
    pub(crate) slot: u64,
    /// Gas used by the block, for mock proving-time simulation.
    pub(crate) gas_used: u64,
    /// When the payload arrived, the start of its proof latency.
    pub(crate) received_at: Instant,
}

impl NewPayloadRequestMeta {
    /// Reads the metadata of a payload request received at `received_at`.
    pub(crate) fn new(payload: &NewPayloadRequest, received_at: Instant) -> anyhow::Result<Self> {
        Ok(Self {
            new_payload_request_root: Hash256::from(payload.hash_tree_root(&Sha2Hasher)),
            block_hash: payload.block_hash(),
            parent_beacon_block_root: payload
                .parent_beacon_block_root()
                .context("payload without parent beacon block root")?,
            block_number: payload.block_number(),
            slot: payload.slot().context("payload without slot")?,
            gas_used: payload.gas_used(),
            received_at,
        })
    }
}

/// A wrapper for `stateless_input_bytes` with the metadata of the request.
#[derive(Debug)]
pub(crate) struct StatelessInput {
    payload_meta: NewPayloadRequestMeta,
    stateless_input_bytes: Vec<u8>,
}

impl StatelessInput {
    /// Builds the `StatelessInput` of a payload request, proven under the rules of the protocol
    /// fork.
    pub(crate) fn new(
        protocol_fork: ProtocolFork,
        payload_meta: NewPayloadRequestMeta,
        payload: NewPayloadRequest,
        witness: ExecutionWitness,
        chain_id: u64,
    ) -> anyhow::Result<Self> {
        let public_keys = PublicKeys::from(recover_public_keys(payload.transactions())?);

        let stateless_input_bytes = stateless_validator_common::guest::StatelessInput {
            new_payload_request: payload,
            witness,
            chain_id,
            public_keys,
        }
        .to_schema_prefixed_ssz(protocol_fork);

        Ok(Self {
            payload_meta,
            stateless_input_bytes,
        })
    }

    /// Returns the metadata of the request of the input.
    pub(crate) fn payload_meta(&self) -> NewPayloadRequestMeta {
        self.payload_meta
    }

    /// Returns the schema-id-prefixed SSZ bytes used as zkVM stdin.
    pub(crate) fn stateless_input_bytes(&self) -> &[u8] {
        &self.stateless_input_bytes
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crate::proof::input::{NewPayloadRequestMeta, StatelessInput};

    /// The stateless input of block 93354 of glamsterdam-devnet-8.
    const AMSTERDAM_STATELESS_INPUT: &[u8] =
        include_bytes!("../../tests/fixture/stateless_input_amsterdam.ssz");

    /// The input built from the decoded fixture encodes to the fixture bytes the guest reads.
    #[test]
    fn test_stateless_input_matches_fixture() {
        let (protocol_fork, fixture) =
            stateless_validator_common::guest::StatelessInput::from_schema_prefixed_ssz(
                AMSTERDAM_STATELESS_INPUT,
            )
            .unwrap();
        let input = StatelessInput::new(
            protocol_fork,
            NewPayloadRequestMeta::new(&fixture.new_payload_request, Instant::now()).unwrap(),
            fixture.new_payload_request,
            fixture.witness,
            fixture.chain_id,
        )
        .unwrap();
        assert_eq!(input.stateless_input_bytes(), AMSTERDAM_STATELESS_INPUT);
    }
}

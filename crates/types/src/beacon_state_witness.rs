//! Top-level SSZ leaves of a post-Gloas `BeaconState`, the beacon anchor of an execution proof.

use alloy_primitives::B256;
use anyhow::ensure;
use libssz_derive::{SszDecode, SszEncode};
use libssz_merkle::{merkleize_progressive, mix_in_active_fields};
use stateless_validator_common::{Sha2Hasher, SszList};

/// Active field count of the Gloas `BeaconState`.
pub const STATE_FIELD_COUNT: usize = 46;

/// Field roots that merkleize to the state root, so a reader checks a value against its leaf.
#[derive(Debug, Clone, PartialEq, Eq, SszEncode, SszDecode)]
pub struct BeaconStateWitness {
    /// Hash tree root of every top-level field, in field order.
    pub leaves: SszList<[u8; 32], STATE_FIELD_COUNT>,
}

impl BeaconStateWitness {
    /// Collects the leaves of a state, which must have the Gloas field count.
    pub fn new(leaves: Vec<[u8; 32]>) -> anyhow::Result<Self> {
        ensure!(
            leaves.len() == STATE_FIELD_COUNT,
            "beacon state has {} top-level fields, not the {STATE_FIELD_COUNT} of Gloas",
            leaves.len()
        );
        Ok(Self {
            leaves: SszList::try_from(leaves).map_err(|error| {
                anyhow::anyhow!("beacon state leaf count out of bounds: {error:?}")
            })?,
        })
    }

    /// Merkleizes the leaves as the `ProgressiveContainer` of a post-Gloas state.
    pub fn state_root(&self) -> B256 {
        B256::from(mix_in_active_fields(
            &Sha2Hasher,
            &merkleize_progressive(&Sha2Hasher, &self.leaves),
            &[true; STATE_FIELD_COUNT],
        ))
    }
}

#[cfg(test)]
mod tests {
    use libssz_merkle::{Sha2Hasher, hash_nodes, merkleize};

    use crate::{BeaconStateWitness, STATE_FIELD_COUNT, SszDecode, SszEncode};

    /// Distinct leaves in field order.
    fn leaves() -> Vec<[u8; 32]> {
        (0..STATE_FIELD_COUNT as u8)
            .map(|field| [field; 32])
            .collect()
    }

    /// The root is the EIP-7916 spine spelled out, with the active field bits mixed in.
    #[test]
    fn test_state_root_is_the_progressive_spine() {
        let leaves = leaves();
        let subtree = |range: core::ops::Range<usize>, limit: usize| {
            merkleize(&Sha2Hasher, &leaves[range], Some(limit))
        };
        let spine = hash_nodes(
            &Sha2Hasher,
            &subtree(0..1, 1),
            &hash_nodes(
                &Sha2Hasher,
                &subtree(1..5, 4),
                &hash_nodes(
                    &Sha2Hasher,
                    &subtree(5..21, 16),
                    &hash_nodes(&Sha2Hasher, &subtree(21..46, 64), &[0; 32]),
                ),
            ),
        );
        let mut active_fields = [0u8; 32];
        for field in 0..STATE_FIELD_COUNT {
            active_fields[field / 8] |= 1 << (field % 8);
        }

        assert_eq!(
            BeaconStateWitness::new(leaves).unwrap().state_root().0,
            hash_nodes(&Sha2Hasher, &spine, &active_fields)
        );
    }

    /// Another field count means another fork, which no beacon block root would match.
    #[test]
    fn test_witness_rejects_another_field_count() {
        assert!(BeaconStateWitness::new(leaves()[..45].to_vec()).is_err());
    }

    /// The witness round-trips through SSZ.
    #[test]
    fn test_witness_ssz_round_trip() {
        let witness = BeaconStateWitness::new(leaves()).unwrap();
        let encoded = witness.to_ssz();
        assert_eq!(
            BeaconStateWitness::from_ssz_bytes(&encoded).unwrap(),
            witness
        );
    }
}

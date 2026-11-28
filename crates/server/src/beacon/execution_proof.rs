//! The EIP-8025 execution proof envelopes, as the beacon node reads them.

use alloy_primitives::B256;
use libssz_derive::{HashTreeRoot, SszDecode, SszEncode};
use sha2::{Digest, Sha256};
use stateless_validator_common::{HashTreeRoot as _, Sha2Hasher, SszList, SszVector};

/// `DOMAIN_EXECUTION_PROOF` of the consensus specs.
const DOMAIN_EXECUTION_PROOF: [u8; 4] = [0x0F, 0, 0, 0];

/// `MAX_PROOF_SIZE` of the consensus specs.
pub const MAX_PROOF_SIZE: usize = 4_194_304;

/// `MAX_EXECUTION_PROOFS_PER_PAYLOAD` of the consensus specs, the bound of one submission.
pub const MAX_EXECUTION_PROOFS_PER_PAYLOAD: usize = 4;

/// `ExecutionProofEnvelope` of the eth-act lighthouse branch `optional-proofs`. It has the
/// `beacon_block_root` in place of the `public_input` of the specs.
#[derive(Debug, Clone, PartialEq, Eq, HashTreeRoot, SszEncode, SszDecode)]
pub struct ExecutionProofEnvelope {
    pub proof_data: SszList<u8, MAX_PROOF_SIZE>,
    pub proof_type: u8,
    pub beacon_block_root: [u8; 32],
}

/// `SignedExecutionProofEnvelope` of the eth-act lighthouse branch `optional-proofs`.
#[derive(Debug, Clone, PartialEq, Eq, HashTreeRoot, SszEncode, SszDecode)]
pub struct SignedExecutionProofEnvelope {
    pub message: ExecutionProofEnvelope,
    pub validator_index: u64,
    pub signature: SszVector<u8, 96>,
}

/// The body of `POST /eth/v1/beacon/execution_proofs`.
pub type SignedExecutionProofEnvelopes =
    SszList<SignedExecutionProofEnvelope, MAX_EXECUTION_PROOFS_PER_PAYLOAD>;

/// The `proof_data` of a mock proof. A verifier of mock proofs returns `public_values` without
/// a check of `proof`.
#[derive(Debug, Clone, PartialEq, Eq, SszEncode, SszDecode)]
pub struct MockProof {
    /// The public values that the guest commits, as a verifier of a real proof returns them.
    pub public_values: Vec<u8>,
    /// Random bytes in place of a real proof.
    pub proof: Vec<u8>,
}

impl ExecutionProofEnvelope {
    /// Returns the signing root under the fork version of the slot of the beacon block.
    pub fn signing_root(&self, fork_version: [u8; 4], genesis_validators_root: B256) -> B256 {
        // `compute_domain` and `compute_signing_root` of the consensus specs.
        let mut fork_version_chunk = [0; 32];
        fork_version_chunk[..4].copy_from_slice(&fork_version);
        let fork_data_root = sha256(fork_version_chunk, genesis_validators_root.0);
        let mut domain = [0; 32];
        domain[..4].copy_from_slice(&DOMAIN_EXECUTION_PROOF);
        domain[4..].copy_from_slice(&fork_data_root[..28]);
        sha256(self.hash_tree_root(&Sha2Hasher), domain)
    }
}

fn sha256(left: [u8; 32], right: [u8; 32]) -> B256 {
    B256::from_slice(
        &Sha256::new()
            .chain_update(left)
            .chain_update(right)
            .finalize(),
    )
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use lighthouse_types::{ChainSpec, SignedRoot};
    use stateless_validator_common::{HashTreeRoot, Sha2Hasher};

    use crate::beacon::execution_proof::ExecutionProofEnvelope;

    /// The signing root agrees with the `ForkData` and `SigningData` containers of lighthouse.
    #[test]
    fn test_signing_root_matches_lighthouse() {
        let fork_version = [0x60, 0x00, 0x00, 0x93];
        let genesis_validators_root = B256::repeat_byte(0x11);
        let envelope = ExecutionProofEnvelope {
            proof_data: vec![1, 2, 3].try_into().unwrap(),
            proof_type: 6,
            beacon_block_root: [0x22; 32],
        };

        let mut domain = [0; 32];
        domain[..4].copy_from_slice(&15u32.to_le_bytes());
        domain[4..].copy_from_slice(
            &ChainSpec::compute_fork_data_root(fork_version, genesis_validators_root)[..28],
        );
        let object_root = B256::from(envelope.hash_tree_root(&Sha2Hasher));
        assert_eq!(
            envelope.signing_root(fork_version, genesis_validators_root),
            object_root.signing_root(domain.into())
        );
    }
}

//! EIP-8025 execution proof envelopes of `POST /eth/v1/beacon/execution_proofs` and the signing
//! helpers of the consensus specs. `proof_data` is a `List[byte, MAX_PROOF_SIZE]`, as lighthouse
//! defines it, not the `ProgressiveList[byte]` of the specs.

use alloy_primitives::B256;
use libssz_derive::{HashTreeRoot, SszDecode, SszEncode};
use libssz_merkle::HashTreeRoot as _;
use sha2::{Digest, Sha256};
use stateless_validator_common::{Sha2Hasher, SszList, SszVector};

/// `DOMAIN_EXECUTION_PROOF` of the consensus specs.
pub const DOMAIN_EXECUTION_PROOF: [u8; 4] = [0x0F, 0, 0, 0];

/// `MAX_PROOF_SIZE` of the consensus specs.
pub const MAX_PROOF_SIZE: usize = 4_194_304;

/// `MAX_EXECUTION_PROOFS_PER_PAYLOAD` of lighthouse, the bound of one submission.
pub const MAX_EXECUTION_PROOFS_PER_PAYLOAD: usize = 4;

/// `ExecutionProofEnvelope` of the consensus specs.
#[derive(Debug, Clone, PartialEq, Eq, HashTreeRoot, SszEncode, SszDecode)]
pub struct ExecutionProofEnvelope {
    /// The opaque proof bytes.
    pub proof_data: SszList<u8, MAX_PROOF_SIZE>,
    /// The execution proof type of the beacon chain.
    pub proof_type: u8,
    /// The root of the beacon block that carries the proven payload.
    pub beacon_block_root: [u8; 32],
}

impl ExecutionProofEnvelope {
    /// Returns the signing root under `domain`.
    pub fn signing_root(&self, domain: B256) -> B256 {
        compute_signing_root(B256::from(self.hash_tree_root(&Sha2Hasher)), domain)
    }
}

/// `SignedExecutionProofEnvelope` of the consensus specs.
#[derive(Debug, Clone, PartialEq, Eq, HashTreeRoot, SszEncode, SszDecode)]
pub struct SignedExecutionProofEnvelope {
    /// The signed envelope.
    pub message: ExecutionProofEnvelope,
    /// The index of the validator that signed the envelope.
    pub validator_index: u64,
    /// The BLS signature of the validator over the signing root of `message`.
    pub signature: SszVector<u8, 96>,
}

/// The body of `POST /eth/v1/beacon/execution_proofs`.
pub type SignedExecutionProofEnvelopes =
    SszList<SignedExecutionProofEnvelope, MAX_EXECUTION_PROOFS_PER_PAYLOAD>;

/// `compute_domain` of the consensus specs for `DOMAIN_EXECUTION_PROOF`.
pub fn execution_proof_domain(fork_version: [u8; 4], genesis_validators_root: B256) -> B256 {
    let mut version_chunk = [0u8; 32];
    version_chunk[..4].copy_from_slice(&fork_version);
    let fork_data_root = Sha256::new()
        .chain_update(version_chunk)
        .chain_update(genesis_validators_root)
        .finalize();
    let mut domain = [0u8; 32];
    domain[..4].copy_from_slice(&DOMAIN_EXECUTION_PROOF);
    domain[4..].copy_from_slice(&fork_data_root[..28]);
    B256::from(domain)
}

/// `compute_signing_root` of the consensus specs.
pub fn compute_signing_root(object_root: B256, domain: B256) -> B256 {
    B256::from_slice(
        &Sha256::new()
            .chain_update(object_root)
            .chain_update(domain)
            .finalize(),
    )
}

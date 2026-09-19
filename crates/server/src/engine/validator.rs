//! The validator that signs every proof as an EIP-8025 envelope under the beacon block that
//! carries the payload. The envelope is signed under the fork at the slot of that block.

use alloy_primitives::B256;
use anyhow::anyhow;
use lighthouse_bls::Keypair;
use lighthouse_types::{ChainSpec, EthSpec, MainnetEthSpec, Slot};
use zkboost_types::{
    ExecutionProofEnvelope, SignedExecutionProofEnvelope, SignedExecutionProofEnvelopes, SszList,
    SszVector, execution_proof_domain,
};

/// Signs every generated proof as a validator.
#[allow(missing_debug_implementations)]
pub(crate) struct Validator {
    keypair: Keypair,
    spec: ChainSpec,
    genesis_validators_root: B256,
    validator_index: u64,
}

impl Validator {
    /// Creates the validator that signs with the keypair under the spec of the beacon node.
    pub(crate) fn new(
        keypair: Keypair,
        spec: ChainSpec,
        genesis_validators_root: B256,
        validator_index: u64,
    ) -> Self {
        Self {
            keypair,
            spec,
            genesis_validators_root,
            validator_index,
        }
    }

    /// Returns the chain id of the EL, the deposit chain id of the spec.
    pub(crate) fn chain_id(&self) -> u64 {
        self.spec.deposit_chain_id
    }

    /// Signs a proof under a beacon block and returns the list with the one signed envelope.
    pub(crate) fn sign_execution_proofs(
        &self,
        beacon_block_root: B256,
        slot: Slot,
        proof_type: u8,
        proof_data: Vec<u8>,
    ) -> anyhow::Result<SignedExecutionProofEnvelopes> {
        let proof_data = SszList::try_from(proof_data)
            .map_err(|error| anyhow!("proof exceeds MAX_PROOF_SIZE: {error:?}"))?;
        let fork = self
            .spec
            .fork_at_epoch(slot.epoch(MainnetEthSpec::slots_per_epoch()));
        let domain = execution_proof_domain(fork.current_version, self.genesis_validators_root);
        let message = ExecutionProofEnvelope {
            proof_data,
            proof_type,
            beacon_block_root: beacon_block_root.0,
        };
        let signing_root = message.signing_root(domain);
        let signature = self
            .keypair
            .sk
            .sign(lighthouse_bls::Hash256::from_slice(signing_root.as_slice()));
        let envelope = SignedExecutionProofEnvelope {
            message,
            validator_index: self.validator_index,
            signature: SszVector::try_from(signature.serialize().to_vec())
                .expect("a BLS signature has 96 bytes"),
        };
        Ok(SszList::try_from(vec![envelope]).expect("one envelope is within the bound"))
    }
}

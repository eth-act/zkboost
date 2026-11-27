//! The validator that signs every proof as an EIP-8025 envelope under the beacon block that
//! carries the payload.

use std::{fs, path::Path};

use alloy_eips::eip7892::BPO2_MAX_BLOBS_PER_BLOCK;
use alloy_primitives::B256;
use anyhow::{Context, anyhow, bail};
use lighthouse_bls::Keypair;
use lighthouse_eth2_keystore::Keystore;
use lighthouse_types::{ChainSpec, EthSpec, MainnetEthSpec, Slot};
use serde::Deserialize;
use stateless_validator_common::{SszEncode, SszList, SszVector, guest::input::ProtocolFork};

use crate::{
    beacon::{
        beacon_client::BeaconClient,
        execution_proof::{
            ExecutionProofEnvelope, SignedExecutionProofEnvelope, SignedExecutionProofEnvelopes,
        },
    },
    proof::proof_type::ProofType,
};

pub(crate) struct Validator {
    keypair: Keypair,
    index: u64,
    spec: ChainSpec,
    genesis_validators_root: B256,
}

#[derive(Deserialize)]
struct ValidatorIndex {
    #[serde(with = "serde_utils::quoted_u64")]
    index: u64,
}

impl Validator {
    /// Reads from the beacon node what the validator of a keypair needs to sign.
    pub(crate) async fn fetch(beacon: &BeaconClient, keypair: Keypair) -> anyhow::Result<Self> {
        let path = format!("eth/v1/beacon/states/head/validators/{}", keypair.pk);
        let validator: ValidatorIndex = beacon.get(&path).await?.context("validator not found")?;
        Ok(Self {
            keypair,
            index: validator.index,
            spec: beacon.spec().await?,
            genesis_validators_root: beacon.genesis_validators_root().await?,
        })
    }

    pub(crate) fn chain_id(&self) -> u64 {
        self.spec.deposit_chain_id
    }

    /// Returns the protocol fork of a payload at a slot. It checks the forks from the latest
    /// supported one, and matches the BPO fork inside it by the max blob count.
    pub(crate) fn protocol_fork(&self, slot: u64) -> anyhow::Result<ProtocolFork> {
        let epoch = Slot::new(slot).epoch(MainnetEthSpec::slots_per_epoch());
        if (self.spec.gloas_fork_epoch).is_some_and(|gloas_fork_epoch| gloas_fork_epoch <= epoch) {
            return match self.spec.max_blobs_per_block(epoch) {
                BPO2_MAX_BLOBS_PER_BLOCK => Ok(ProtocolFork::Amsterdam),
                max_blobs => bail!("slot {slot} has the max blob count {max_blobs} of no BPO fork"),
            };
        }
        bail!("slot {slot} is before the Gloas fork")
    }

    /// Signs a proof under a beacon block and returns the SSZ body of the submission.
    pub(crate) fn sign(
        &self,
        beacon_block_root: B256,
        slot: u64,
        proof_type: ProofType,
        proof: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        let message = ExecutionProofEnvelope {
            proof_data: SszList::try_from(proof.to_vec())
                .map_err(|error| anyhow!("proof exceeds MAX_PROOF_SIZE: {error:?}"))?,
            proof_type: proof_type as u8,
            beacon_block_root: beacon_block_root.0,
        };
        let epoch = Slot::new(slot).epoch(MainnetEthSpec::slots_per_epoch());
        let fork_version = self.spec.fork_at_epoch(epoch).current_version;
        let signing_root = message.signing_root(fork_version, self.genesis_validators_root);
        let signature = self.keypair.sk.sign(signing_root.0.into());
        let envelope = SignedExecutionProofEnvelope {
            message,
            validator_index: self.index,
            signature: SszVector::try_from(signature.serialize().to_vec())
                .expect("a BLS signature has 96 bytes"),
        };
        let envelopes: SignedExecutionProofEnvelopes =
            SszList::try_from(vec![envelope]).expect("one envelope is within the bound");
        Ok(envelopes.to_ssz())
    }
}

/// Reads the keypair of the validator from an EIP-2335 keystore and its password file.
/// Trailing newlines are not part of the password, as in lighthouse.
pub(crate) fn read_keypair(keystore_path: &Path, password_path: &Path) -> anyhow::Result<Keypair> {
    let password_bytes =
        fs::read(password_path).with_context(|| format!("read {}", password_path.display()))?;
    let mut password = password_bytes.as_slice();
    while let [rest @ .., b'\n' | b'\r'] = password {
        password = rest;
    }
    Keystore::from_json_file(keystore_path)
        .map_err(|error| anyhow!("read keystore {}: {error:?}", keystore_path.display()))?
        .decrypt_keypair(password)
        .map_err(|error| anyhow!("decrypt keystore {}: {error:?}", keystore_path.display()))
}

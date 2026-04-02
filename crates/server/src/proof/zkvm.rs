//! zkVM instance management and initialization, supporting external Ere servers via HTTP and
//! in-process mock instances for testing.

use std::{ops::Deref, sync::Arc, time::Duration};

use anyhow::Context;
use ere_server::client::zkVMClient;
use ere_zkvm_interface::{Input, Proof, ProofKind, PublicValues};
use rand::{Rng, rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stateless_validator_common::new_payload_request::NewPayloadRequest;
use stateless_validator_ethrex::guest::{
    StatelessValidatorEthrexGuest, StatelessValidatorEthrexIo,
};
use stateless_validator_reth::guest::{
    Guest, Io, Platform, StatelessValidatorOutput, StatelessValidatorRethGuest,
    StatelessValidatorRethIo,
};
use tokio::time::{Instant, sleep_until};
use tracing::warn;
use url::Url;
use zkboost_types::{ElKind, Hash256, ProofType};

use crate::{
    config::{MockProvingTime, zkVMConfig},
    proof::input::NewPayloadRequestWithWitness,
};

#[derive(Debug, thiserror::Error)]
#[allow(non_camel_case_types)]
pub(crate) enum zkVMError {
    /// The proof could not be verified by the zkVM backend.
    #[error("proof verification failed: {0}")]
    VerificationFailed(String),
    /// The public values do not match the expected values.
    #[error("public values mismatch")]
    PublicValuesMismatch,
}

/// zkVM instance, either a remote ere-server or a mock.
#[allow(non_camel_case_types)]
#[derive(Clone, Debug)]
pub(crate) enum zkVMInstance {
    /// External Ere server that provides zkVM functionalities via HTTP endpoints.
    External {
        /// Proof type identifier (e.g. `"reth-sp1"`).
        proof_type: ProofType,
        /// Client of external Ere server.
        client: Arc<zkVMClient>,
    },
    /// Mock zkVM for testing.
    Mock {
        /// Proof type identifier (e.g. `"reth-sp1"`).
        proof_type: ProofType,
        /// Mock zkVM implementation.
        vm: MockzkVM,
    },
}

impl zkVMInstance {
    /// Creates a new zkVM instance from configuration.
    pub(crate) async fn new(config: &zkVMConfig) -> anyhow::Result<Self> {
        match config {
            zkVMConfig::External {
                endpoint,
                proof_type,
            } => {
                let endpoint_url = Url::parse(endpoint)
                    .with_context(|| format!("Failed to parse endpoint URL: {endpoint}"))?;
                let client =
                    zkVMClient::from_endpoint(endpoint_url.clone()).with_context(|| {
                        format!("Failed to create zkVM client for endpoint: {endpoint_url}")
                    })?;
                Ok(Self::External {
                    proof_type: *proof_type,
                    client: Arc::new(client),
                })
            }
            zkVMConfig::Mock {
                proof_type,
                mock_proving_time,
                mock_proof_size,
                mock_failure,
            } => Ok(Self::Mock {
                proof_type: *proof_type,
                vm: MockzkVM::new(
                    proof_type.el_kind(),
                    mock_proving_time.clone(),
                    *mock_proof_size,
                    *mock_failure,
                ),
            }),
        }
    }

    /// Generates a compressed proof for the given payload, returning raw proof bytes.
    pub(crate) async fn prove(
        &self,
        new_payload_request_with_witness: &NewPayloadRequestWithWitness,
    ) -> anyhow::Result<Vec<u8>> {
        let el_kind = self.proof_type().el_kind();
        let input = new_payload_request_with_witness.to_zkvm_input(el_kind)?;
        match self {
            Self::External { client, .. } => {
                let (_, proof, _) = client.prove(input, ProofKind::Compressed).await?;
                match proof {
                    Proof::Compressed(bytes) => Ok(bytes),
                    _ => anyhow::bail!("unexpected proof kind: {:?}", proof.kind()),
                }
            }
            Self::Mock { vm, .. } => vm.prove(&input).await,
        }
    }

    /// Verifies a compressed proof against the expected public values.
    pub(crate) async fn verify(
        &self,
        new_payload_request_root: Hash256,
        proof: Vec<u8>,
    ) -> Result<(), zkVMError> {
        let public_values = match self {
            Self::External { client, .. } => client
                .verify(Proof::Compressed(proof))
                .await
                .map_err(|error| zkVMError::VerificationFailed(error.to_string())),
            Self::Mock { vm, .. } => vm
                .verify(&proof)
                .await
                .map_err(|error| zkVMError::VerificationFailed(error.to_string())),
        }?;

        let el_kind = self.proof_type().el_kind();
        let expected = expected_public_values(new_payload_request_root, el_kind)
            .map_err(|error| zkVMError::VerificationFailed(error.to_string()))?;

        // For zkVM with fixed size public values, ensure all padding are zeros.
        if public_values.len() >= 32
            && public_values[..32] == expected
            && public_values[32..].iter().all(|byte| *byte == 0)
        {
            Ok(())
        } else {
            warn!(?public_values, ?expected, "unexpected public values");
            Err(zkVMError::PublicValuesMismatch)
        }
    }

    /// Returns the proof type identifier for this instance.
    pub(crate) fn proof_type(&self) -> ProofType {
        match self {
            Self::External { proof_type, .. } | Self::Mock { proof_type, .. } => *proof_type,
        }
    }
}

/// Serializable mock proof used by `MockzkVM` for testing.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MockProof {
    public_values: PublicValues,
    proof: Vec<u8>,
}

impl MockProof {
    /// Returns a `MockProof` with random proof bytes.
    pub(crate) fn new(public_values: PublicValues, mock_proof_size: u64) -> Self {
        let mut proof = vec![0; mock_proof_size as usize];
        rand::fill(proof.as_mut_slice());
        Self {
            public_values,
            proof,
        }
    }
}

/// Mock zkVM for testing.
#[derive(Debug, Clone)]
pub(crate) struct MockzkVM {
    el_kind: ElKind,
    mock_proving_time: MockProvingTime,
    mock_proof_size: u64,
    failure: bool,
}

impl MockzkVM {
    /// Construct a `MockzkVM`.
    pub(crate) fn new(
        el_kind: ElKind,
        mock_proving_time: MockProvingTime,
        mock_proof_size: u64,
        failure: bool,
    ) -> Self {
        Self {
            el_kind,
            mock_proving_time,
            mock_proof_size,
            failure,
        }
    }

    /// Simulate proof generation with configurable delay, returning raw proof bytes.
    pub(crate) async fn prove(&self, input: &Input) -> anyhow::Result<Vec<u8>> {
        let start = Instant::now();

        let (hash, gas_used) = execute(self.el_kind, input)?;
        let public_values = hash.to_vec();

        let duration = match &self.mock_proving_time {
            MockProvingTime::Constant { ms } => Duration::from_millis(*ms),
            MockProvingTime::Random { min_ms, max_ms } => {
                Duration::from_millis(rng().random_range(*min_ms..=*max_ms))
            }
            MockProvingTime::Linear { ms_per_mgas } => {
                Duration::from_millis(ms_per_mgas.saturating_mul(gas_used).div_ceil(1_000_000))
            }
        };
        sleep_until(start + duration).await;

        if self.failure {
            anyhow::bail!("proof generation failure");
        }

        Ok(bincode::serialize(&MockProof::new(
            public_values,
            self.mock_proof_size,
        ))?)
    }

    /// Simulate proof verification by checking proof size.
    pub(crate) async fn verify(&self, proof: &[u8]) -> anyhow::Result<PublicValues> {
        let start = Instant::now();

        let mock_proof: MockProof = bincode::deserialize(proof)?;

        let duration = Duration::from_millis(10);
        sleep_until(start + duration).await;

        if mock_proof.proof.len() == self.mock_proof_size as usize {
            Ok(mock_proof.public_values)
        } else {
            anyhow::bail!("invalid proof")
        }
    }
}

fn execute(el_kind: ElKind, input: &Input) -> anyhow::Result<([u8; 32], u64)> {
    struct Host;

    impl Platform for Host {
        fn read_whole_input() -> impl Deref<Target = [u8]> {
            [].as_slice()
        }

        fn write_whole_output(_: &[u8]) {}

        fn print(_: &str) {}
    }

    let (_, stdin) = input
        .stdin
        .split_at_checked(4)
        .ok_or_else(|| anyhow::anyhow!("stdin should have length prefixed"))?;

    match el_kind {
        ElKind::Ethrex => {
            let input = StatelessValidatorEthrexIo::deserialize_input(stdin)?;
            let gas_used = gas_used(&input.new_payload_request);
            let output = StatelessValidatorEthrexGuest::compute::<Host>(input);
            let serialized = StatelessValidatorEthrexIo::serialize_output(&output)?;
            Ok((Sha256::digest(serialized).into(), gas_used))
        }
        ElKind::Reth => {
            let input = StatelessValidatorRethIo::deserialize_input(stdin)?;
            let gas_used = gas_used(&input.new_payload_request);
            let output = StatelessValidatorRethGuest::compute::<Host>(input);
            let serialized = StatelessValidatorRethIo::serialize_output(&output)?;
            Ok((Sha256::digest(serialized).into(), gas_used))
        }
    }
}

fn gas_used(req: &NewPayloadRequest) -> u64 {
    match req {
        NewPayloadRequest::Bellatrix(r) => r.execution_payload.gas_used,
        NewPayloadRequest::Capella(r) => r.execution_payload.gas_used,
        NewPayloadRequest::Deneb(r) => r.execution_payload.gas_used,
        NewPayloadRequest::ElectraFulu(r) => r.execution_payload.gas_used,
    }
}

/// Computes the expected public values hash for a given payload root and EL kind.
pub(crate) fn expected_public_values(
    new_payload_request_root: Hash256,
    el_kind: ElKind,
) -> anyhow::Result<[u8; 32]> {
    let output = StatelessValidatorOutput::new(new_payload_request_root.0, true);
    let serialized = match el_kind {
        ElKind::Reth => StatelessValidatorRethIo::serialize_output(&output)?,
        ElKind::Ethrex => StatelessValidatorEthrexIo::serialize_output(&output)?,
    };
    Ok(Sha256::digest(serialized).into())
}

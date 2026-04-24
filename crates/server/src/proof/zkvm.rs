//! zkVM instance management and initialization, supporting external Ere servers via HTTP and
//! in-process mock instances for testing.

use std::{ops::Deref, sync::Arc, time::Duration};

use anyhow::Context;
use ere_guests_stateless_validator_common::guest::StatelessValidatorOutput;
use ere_guests_stateless_validator_ethrex::{
    guest::StatelessValidatorEthrexGuest, host::build_eip8025_input,
};
use ere_guests_stateless_validator_reth::guest::{
    Guest, Platform, StatelessValidatorRethGuest, StatelessValidatorRethInput, codec::Encode,
};
use ere_server_client::{EncodedProof, PublicValues, zkVMClient};
use ere_verifier::Verifier;
use rand::{Rng, rng};
use sha2::{Digest, Sha256};
use stateless::StatelessInput;
use tokio::time::{Instant, sleep, sleep_until};
use tracing::warn;
use url::Url;
use zkboost_types::{ElKind, Hash256, ProofType};

use crate::{
    config::{MockProvingTime, zkVMConfig},
    proof::{input::NewPayloadRequestWithWitness, verifier::verifier_from_url},
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

/// zkVM instance: remote ere-server, in-process mock, or in-process verifier-only.
#[allow(non_camel_case_types)]
#[derive(Clone, Debug)]
pub(crate) enum zkVMInstance {
    /// External Ere server that provides zkVM functionalities via HTTP endpoints.
    Ere {
        /// Proof type identifier (e.g. `"reth-sp1"`).
        proof_type: ProofType,
        /// Timeout for proof generation.
        proof_timeout: Duration,
        /// Client of external Ere server.
        client: Arc<zkVMClient>,
    },
    /// Mock zkVM for testing.
    Mock {
        /// Proof type identifier (e.g. `"reth-sp1"`).
        proof_type: ProofType,
        /// Timeout for proof generation.
        proof_timeout: Duration,
        /// Mock zkVM implementation.
        vm: MockzkVM,
    },
    /// In-process verifier-only backend. No `ere-server`, no prover circuit
    /// loaded — just the lightweight `ere-verifier-*` for this proof type.
    /// Returns an error on prove requests.
    Verifier {
        /// Proof type identifier.
        proof_type: ProofType,
        /// Verifier implementation, dispatched per proof_type.
        verifier: Arc<Verifier>,
    },
}

impl zkVMInstance {
    /// Creates a new zkVM instance from configuration.
    pub(crate) async fn new(config: &zkVMConfig) -> anyhow::Result<Self> {
        match config {
            zkVMConfig::Ere {
                proof_type,
                proof_timeout_secs,
                endpoint,
            } => {
                let endpoint_url = Url::parse(endpoint)
                    .with_context(|| format!("failed to parse endpoint URL: {endpoint}"))?;
                let client = {
                    #[cfg(feature = "otel")]
                    let middlewares = vec![Box::new(ere_server_client::OtelPropagation) as Box<_>];
                    #[cfg(not(feature = "otel"))]
                    let middlewares = Vec::new();

                    zkVMClient::new(endpoint_url.clone(), reqwest::Client::new(), middlewares)
                        .with_context(|| {
                            format!("failed to create zkVM client for endpoint: {endpoint_url}")
                        })?
                };
                Ok(Self::Ere {
                    proof_type: *proof_type,
                    proof_timeout: Duration::from_secs(*proof_timeout_secs),
                    client: Arc::new(client),
                })
            }
            zkVMConfig::Mock {
                proof_type,
                proof_timeout_secs,
                mock_proving_time,
                mock_proof_size,
                mock_failure,
            } => Ok(Self::Mock {
                proof_type: *proof_type,
                proof_timeout: Duration::from_secs(*proof_timeout_secs),
                vm: MockzkVM::new(
                    proof_type.el_kind(),
                    mock_proving_time.clone(),
                    *mock_proof_size,
                    *mock_failure,
                ),
            }),
            zkVMConfig::Verifier {
                proof_type,
                program_vk_url,
            } => {
                let verifier = verifier_from_url(*proof_type, program_vk_url)
                    .await
                    .with_context(|| {
                        format!("init in-process verifier for {proof_type} from {program_vk_url}")
                    })?;
                Ok(Self::Verifier {
                    proof_type: *proof_type,
                    verifier: Arc::new(verifier),
                })
            }
        }
    }

    /// Generates a compressed proof for the given payload, returning raw proof bytes.
    pub(crate) async fn prove(
        &self,
        new_payload_request_with_witness: &NewPayloadRequestWithWitness,
    ) -> anyhow::Result<Vec<u8>> {
        if let Self::Mock { vm, .. } = self {
            return vm
                .prove(new_payload_request_with_witness.stateless_input())
                .await;
        }
        if let Self::Verifier { proof_type, .. } = self {
            anyhow::bail!("prove not supported for verifier-only zkvm {proof_type}");
        }

        let el_kind = self.proof_type().el_kind();
        let input = new_payload_request_with_witness.to_zkvm_input(el_kind)?;
        match self {
            Self::Ere { client, .. } => {
                let (_, proof, _) = client.prove(input).await?;
                Ok(proof.0)
            }
            Self::Mock { .. } | Self::Verifier { .. } => unreachable!(),
        }
    }

    /// Verifies a compressed proof against the expected public values.
    pub(crate) async fn verify(
        &self,
        new_payload_request_root: Hash256,
        proof: Vec<u8>,
    ) -> Result<(), zkVMError> {
        let public_values: PublicValues = match self {
            Self::Ere { client, .. } => client
                .verify(EncodedProof(proof))
                .await
                .map_err(|error| zkVMError::VerificationFailed(error.to_string())),
            Self::Mock { vm, .. } => vm
                .verify(&proof)
                .await
                .map_err(|error| zkVMError::VerificationFailed(error.to_string())),
            Self::Verifier { verifier, .. } => verifier
                .verify(&proof)
                .map_err(|error| zkVMError::VerificationFailed(error.to_string())),
        }?;

        let expected = expected_public_values(new_payload_request_root)
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
            Self::Ere { proof_type, .. }
            | Self::Mock { proof_type, .. }
            | Self::Verifier { proof_type, .. } => *proof_type,
        }
    }

    /// Returns the proof timeout for this instance.
    /// Verifier-only backends never prove, so the timeout is irrelevant — we
    /// return the default to keep the signature uniform.
    pub(crate) fn proof_timeout(&self) -> Duration {
        match self {
            Self::Ere { proof_timeout, .. } | Self::Mock { proof_timeout, .. } => *proof_timeout,
            Self::Verifier { .. } => Duration::from_secs(12),
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
        assert!(mock_proof_size >= 32);
        if let MockProvingTime::Random { min_ms, max_ms, .. } = mock_proving_time {
            assert!(min_ms <= max_ms);
        }
        Self {
            el_kind,
            mock_proving_time,
            mock_proof_size,
            failure,
        }
    }

    /// Simulate proof generation with configurable delay, returning raw proof bytes.
    pub(crate) async fn prove(&self, input: &StatelessInput) -> anyhow::Result<Vec<u8>> {
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
            anyhow::bail!("mocking failure");
        }

        let mut proof = public_values;
        proof.resize(self.mock_proof_size as usize, 0);
        rand::fill(&mut proof[32..]);
        Ok(proof)
    }

    /// Simulate proof verification by checking proof size.
    pub(crate) async fn verify(&self, proof: &[u8]) -> anyhow::Result<PublicValues> {
        sleep(Duration::from_millis(10)).await;

        if proof.len() >= 32 {
            Ok(proof[..32].into())
        } else {
            anyhow::bail!("invalid proof")
        }
    }
}

fn execute(el_kind: ElKind, input: &StatelessInput) -> anyhow::Result<([u8; 32], u64)> {
    struct Host;

    impl Platform for Host {
        fn read_whole_input() -> impl Deref<Target = [u8]> {
            [].as_slice()
        }

        fn write_whole_output(_: &[u8]) {}

        fn print(_: &str) {}
    }

    let public_values = match el_kind {
        ElKind::Ethrex => {
            let input = build_eip8025_input(input, true)?;
            let output = StatelessValidatorEthrexGuest::compute::<Host>(input);
            let serialized = output.encode_to_vec()?;
            Sha256::digest(serialized).into()
        }
        ElKind::Reth => {
            let input = StatelessValidatorRethInput::new(input, true)?;
            let output = StatelessValidatorRethGuest::compute::<Host>(input);
            let serialized = output.encode_to_vec()?;
            Sha256::digest(serialized).into()
        }
    };
    Ok((public_values, input.block.header.gas_used))
}

/// Computes the expected public values hash for a given payload root.
pub(crate) fn expected_public_values(
    new_payload_request_root: Hash256,
) -> anyhow::Result<[u8; 32]> {
    let output = StatelessValidatorOutput::new(new_payload_request_root.0, true);
    let serialized = output.encode_to_vec()?;
    Ok(Sha256::digest(serialized).into())
}

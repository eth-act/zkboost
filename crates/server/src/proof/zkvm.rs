//! zkVM instance management and initialization, supporting external Ere servers via HTTP,
//! in-process mock instances for testing, and remote clusters.

use std::{ops::Deref, sync::Arc, time::Duration};

use anyhow::Context;
use ere_guests_stateless_validator_common::guest::StatelessValidationResult;
use ere_platform_core::Platform;
use ere_server_client::{EncodedProof, Input, PublicValues, zkVMClient};
use ere_verifier::Verifier;
use rand::{Rng, rng};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep, sleep_until};
use url::Url;
use zkboost_types::{ChainConfig, ElKind, Hash256, ProofType, SszEncode};

use crate::{
    config::{MockProvingTime, load, zkVMConfig},
    proof::{input::StatelessInput, zkvm::cluster_client::ClusterClient},
};

mod cluster_client;

/// zkVM instance: remote ere-server, in-process mock, or in-process verifier-only, or a remote
/// cluster.
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
    /// External cluster, currently only supports ZisK.
    Cluster {
        /// Proof type identifier.
        proof_type: ProofType,
        /// Timeout for proof generation.
        proof_timeout: Duration,
        /// Client for the external proving cluster.
        client: ClusterClient,
        /// In-process verifier.
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
                program_vk_path,
                program_vk_url,
            } => {
                let encoded_program_vk = load(program_vk_path, program_vk_url).await?;
                let verifier = Verifier::new(proof_type.zkvm_kind(), &encoded_program_vk)
                    .with_context(|| format!("init in-process verifier for {proof_type}"))?;
                Ok(Self::Verifier {
                    proof_type: *proof_type,
                    verifier: Arc::new(verifier),
                })
            }
            zkVMConfig::Cluster {
                proof_type,
                proof_timeout_secs,
                endpoint,
                elf_path,
                elf_url,
            } => {
                let elf = load(elf_path, elf_url)
                    .await
                    .with_context(|| format!("failed to load cluster elf for {proof_type}"))?;
                let client = ClusterClient::new(*proof_type, endpoint, elf).await?;
                let verifier = Verifier::new(proof_type.zkvm_kind(), &client.program_vk()?)
                    .with_context(|| format!("init in-process verifier for {proof_type}"))?;
                Ok(Self::Cluster {
                    proof_type: *proof_type,
                    proof_timeout: Duration::from_secs(*proof_timeout_secs),
                    client,
                    verifier: Arc::new(verifier),
                })
            }
        }
    }

    /// Generates a compressed proof for the given payload, returning raw proof bytes.
    ///
    /// The attempt is unbounded here. The per-zkVM worker wraps this call in
    /// [`tokio::time::timeout`] using [`proof_timeout`](Self::proof_timeout), so a
    /// timeout drops this future. For the cluster backend the in-flight job is
    /// then cancelled server-side when its `ClusterProveJob` guard is dropped.
    pub(crate) async fn prove(&self, stateless_input: &StatelessInput) -> anyhow::Result<Vec<u8>> {
        if let Self::Mock { vm, .. } = self {
            return vm.prove(stateless_input).await;
        }
        if let Self::Verifier { proof_type, .. } = self {
            anyhow::bail!("prove not supported for verifier-only zkvm {proof_type}");
        }

        let input = Input::new().with_stdin(stateless_input.stateless_input_bytes().to_vec());
        match self {
            Self::Ere { client, .. } => {
                let (_, proof, _) = client.prove(input).await?;
                Ok(proof.0)
            }
            Self::Cluster { client, .. } => client.create_prove_job(&input).await?.wait().await,
            Self::Mock { .. } | Self::Verifier { .. } => unreachable!(),
        }
    }

    /// Verifies a compressed proof against the expected public values.
    pub(crate) async fn verify(
        &self,
        new_payload_request_root: Hash256,
        chain_config: &ChainConfig,
        proof: Vec<u8>,
    ) -> anyhow::Result<()> {
        let public_values: PublicValues = match self {
            Self::Ere { client, .. } => client.verify(EncodedProof(proof)).await?,
            Self::Mock { vm, .. } => vm.verify(&proof).await?,
            Self::Verifier { verifier, .. } | Self::Cluster { verifier, .. } => {
                verifier.verify(&proof)?
            }
        };

        let expected = expected_public_values(new_payload_request_root, chain_config);
        let len = expected.len();

        // For zkVM with fixed size public values, ensure all padding after the
        // SSZ-encoded result are zeros.
        if public_values.len() >= len
            && public_values[..len] == expected[..]
            && public_values[len..].iter().all(|byte| *byte == 0)
        {
            Ok(())
        } else {
            anyhow::bail!("unexpected public values, expected {expected:?}, got: {public_values:?}")
        }
    }

    /// Returns the proof type identifier for this instance.
    pub(crate) fn proof_type(&self) -> ProofType {
        match self {
            Self::Ere { proof_type, .. }
            | Self::Mock { proof_type, .. }
            | Self::Verifier { proof_type, .. } => *proof_type,
            Self::Cluster { proof_type, .. } => *proof_type,
        }
    }

    /// Returns the proof timeout for this instance.
    /// Verifier-only backends never prove, so the timeout is irrelevant — we
    /// return the default to keep the signature uniform.
    pub(crate) fn proof_timeout(&self) -> Duration {
        match self {
            Self::Ere { proof_timeout, .. }
            | Self::Mock { proof_timeout, .. }
            | Self::Cluster { proof_timeout, .. } => *proof_timeout,
            Self::Verifier { .. } => Duration::from_secs(12),
        }
    }

    /// Returns the backend kind and capabilities for this instance.
    ///
    /// - `Ere`: can prove and verify (remote prover)
    /// - `Mock`: can prove and verify (testing)
    /// - `Verifier`: can only verify (no proving circuit loaded)
    /// - `Cluster`: can prove and verify (external proving cluster)
    pub(crate) fn backend_capabilities(&self) -> (zkboost_types::BackendKind, bool, bool) {
        match self {
            Self::Ere { .. } => (zkboost_types::BackendKind::Ere, true, true),
            Self::Mock { .. } => (zkboost_types::BackendKind::Mock, true, true),
            Self::Verifier { .. } => (zkboost_types::BackendKind::Verifier, false, true),
            Self::Cluster { .. } => (zkboost_types::BackendKind::Cluster, true, true),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MockProof {
    public_values: Vec<u8>,
    proof: Vec<u8>,
}

impl MockProof {
    pub(crate) fn new(public_values: Vec<u8>, proof_size: usize) -> Self {
        let proof = rng()
            .random_iter()
            .take(proof_size.saturating_sub(public_values.len() + 16))
            .collect();
        Self {
            public_values,
            proof,
        }
    }

    pub(crate) fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        Ok(bincode::serialize(&self)?)
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(bincode::deserialize(bytes)?)
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

        let stateless_output_bytes = execute(self.el_kind, input.stateless_input_bytes())?;
        let gas_used = input.gas_used();

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

        MockProof::new(stateless_output_bytes, self.mock_proof_size as usize).to_bytes()
    }

    /// Simulate proof verification by decoding the mock proof into its public values.
    pub(crate) async fn verify(&self, proof: &[u8]) -> anyhow::Result<PublicValues> {
        sleep(Duration::from_millis(10)).await;

        Ok(MockProof::from_bytes(proof)?.public_values.into())
    }
}

fn execute(el_kind: ElKind, input_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    struct Host;

    impl Platform for Host {
        fn read_input() -> impl Deref<Target = [u8]> {
            [].as_slice()
        }

        fn write_output(_: &[u8]) {}
    }

    let output_bytes = match el_kind {
        ElKind::Reth => {
            ere_guests_stateless_validator_reth::guest::run_stateless_guest::<Host>(input_bytes)
        }
        ElKind::Ethrex => {
            ere_guests_stateless_validator_ethrex::guest::run_stateless_guest::<Host>(input_bytes)
        }
    };
    Ok(output_bytes)
}

/// Computes the expected public values for a successful validation of the given payload root
/// and chain config.
pub(crate) fn expected_public_values(
    new_payload_request_root: Hash256,
    chain_config: &ChainConfig,
) -> Vec<u8> {
    StatelessValidationResult::new(new_payload_request_root.0, true, chain_config.clone()).to_ssz()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ere_verifier::{Verifier, zkVMKind};
    use zkboost_types::{BackendKind, ProofType};

    use super::*;

    /// Creates a test Ere instance with dummy client.
    fn test_ere_instance() -> zkVMInstance {
        let endpoint = Url::parse("http://localhost:9999").unwrap();
        let client = zkVMClient::new(endpoint, reqwest::Client::new(), vec![]).unwrap();
        zkVMInstance::Ere {
            proof_type: ProofType::RethZisk,
            proof_timeout: Duration::from_secs(10),
            client: Arc::new(client),
        }
    }

    /// Creates a test Mock instance.
    fn test_mock_instance() -> zkVMInstance {
        zkVMInstance::Mock {
            proof_type: ProofType::RethZisk,
            proof_timeout: Duration::from_secs(10),
            vm: MockzkVM::new(
                zkboost_types::ElKind::Reth,
                crate::config::MockProvingTime::Constant { ms: 10 },
                64,
                false,
            ),
        }
    }

    /// Creates a test Verifier instance.
    fn test_verifier_instance() -> zkVMInstance {
        zkVMInstance::Verifier {
            proof_type: ProofType::RethZisk,
            verifier: Arc::new(Verifier::new(zkVMKind::Zisk, &[0; 32]).unwrap()),
        }
    }

    #[test]
    fn test_ere_backend_capabilities() {
        let instance = test_ere_instance();
        let (kind, can_prove, can_verify) = instance.backend_capabilities();

        assert_eq!(kind, BackendKind::Ere);
        assert!(can_prove, "ere backends can prove");
        assert!(can_verify, "ere backends can verify");
    }

    #[test]
    fn test_mock_backend_capabilities() {
        let instance = test_mock_instance();
        let (kind, can_prove, can_verify) = instance.backend_capabilities();

        assert_eq!(kind, BackendKind::Mock);
        assert!(can_prove, "mock backends can prove");
        assert!(can_verify, "mock backends can verify");
    }

    #[test]
    fn test_verifier_backend_capabilities() {
        let instance = test_verifier_instance();
        let (kind, can_prove, can_verify) = instance.backend_capabilities();

        assert_eq!(kind, BackendKind::Verifier);
        assert!(!can_prove, "verifier backends can not prove");
        assert!(can_verify, "verifier backends can verify");
    }
}

//! zkVM instance management and initialization, supporting external Ere servers via HTTP,
//! in-process mock instances for testing, and remote clusters.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use ere_server_client::{Input, zkVMClient};
use rand::{Rng, rng};
use tokio::time::{Instant, sleep_until};
use zkboost_types::{MOCK_PROOF, ProofType};

use crate::{
    config::{MockProvingTime, load, zkVMConfig},
    proof::{input::StatelessInput, zkvm::cluster_client::ClusterClient},
};

mod cluster_client;

/// zkVM instance, one of a remote ere-server, an in-process mock, or a remote cluster.
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
    /// External cluster, currently only supports ZisK.
    Cluster {
        /// Proof type identifier.
        proof_type: ProofType,
        /// Timeout for proof generation.
        proof_timeout: Duration,
        /// Client for the external proving cluster.
        client: ClusterClient,
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
                let client = {
                    #[cfg(feature = "otel")]
                    let middlewares = vec![Box::new(ere_server_client::OtelPropagation) as Box<_>];
                    #[cfg(not(feature = "otel"))]
                    let middlewares = Vec::new();

                    zkVMClient::new(endpoint.clone(), reqwest::Client::new(), middlewares)
                        .with_context(|| {
                            format!("failed to create zkVM client for endpoint: {endpoint}")
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
                mock_failure,
            } => Ok(Self::Mock {
                proof_type: *proof_type,
                proof_timeout: Duration::from_secs(*proof_timeout_secs),
                vm: MockzkVM::new(mock_proving_time.clone(), *mock_failure),
            }),
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
                Ok(Self::Cluster {
                    proof_type: *proof_type,
                    proof_timeout: Duration::from_secs(*proof_timeout_secs),
                    client,
                })
            }
        }
    }

    /// Generates a compressed proof for the given payload, returning raw proof bytes.
    ///
    /// `prove_span` is the worker's prove span. Backends that learn a backend-side job
    /// identifier (currently the cluster) record it there explicitly.
    ///
    /// The attempt is unbounded here. The per-zkVM worker wraps this call in
    /// [`tokio::time::timeout`] using [`proof_timeout`](Self::proof_timeout), so a
    /// timeout drops this future. For the cluster backend the in-flight job is
    /// then cancelled server-side when its `ClusterProveJob` guard is dropped.
    pub(crate) async fn prove(
        &self,
        stateless_input: &StatelessInput,
        prove_span: &tracing::Span,
    ) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::Ere { client, .. } => {
                let input =
                    Input::new().with_stdin(stateless_input.stateless_input_bytes().to_vec());
                let (_, proof, _) = client.prove(input).await?;
                Ok(proof.0)
            }
            Self::Mock { vm, .. } => vm.prove(stateless_input).await,
            Self::Cluster { client, .. } => {
                let input =
                    Input::new().with_stdin(stateless_input.stateless_input_bytes().to_vec());
                client
                    .create_prove_job(&input, prove_span)
                    .await?
                    .wait()
                    .await
            }
        }
    }

    /// Returns the proof type identifier for this instance.
    pub(crate) fn proof_type(&self) -> ProofType {
        match self {
            Self::Ere { proof_type, .. }
            | Self::Mock { proof_type, .. }
            | Self::Cluster { proof_type, .. } => *proof_type,
        }
    }

    /// Returns the proof timeout for this instance.
    pub(crate) fn proof_timeout(&self) -> Duration {
        match self {
            Self::Ere { proof_timeout, .. }
            | Self::Mock { proof_timeout, .. }
            | Self::Cluster { proof_timeout, .. } => *proof_timeout,
        }
    }
}

/// Mock zkVM for testing.
#[derive(Debug, Clone)]
pub(crate) struct MockzkVM {
    mock_proving_time: MockProvingTime,
    failure: bool,
}

impl MockzkVM {
    /// Constructs a `MockzkVM`.
    pub(crate) fn new(mock_proving_time: MockProvingTime, failure: bool) -> Self {
        if let MockProvingTime::Random { min_ms, max_ms, .. } = mock_proving_time {
            assert!(min_ms <= max_ms);
        }
        Self {
            mock_proving_time,
            failure,
        }
    }

    /// Simulates proof generation with configurable delay, returning [`MOCK_PROOF`].
    pub(crate) async fn prove(&self, input: &StatelessInput) -> anyhow::Result<Vec<u8>> {
        let start = Instant::now();
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

        Ok(MOCK_PROOF.to_vec())
    }
}

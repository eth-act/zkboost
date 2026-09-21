//! zkVM instance management and initialization, supporting external Ere servers via HTTP,
//! in-process mock instances for testing, and remote clusters.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use ere_server_client::{Input, zkVMClient};
use rand::{Rng, rng};
use tokio::time::{Instant, sleep_until};
use url::Url;
use zkboost_types::ProofType;

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
    /// External cluster of a ZisK or OpenVM guest.
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
            } => Ok(Self::Ere {
                proof_type: *proof_type,
                proof_timeout: Duration::from_secs(*proof_timeout_secs),
                client: Arc::new(zkvm_client(endpoint)?),
            }),
            zkVMConfig::Mock {
                proof_type,
                proof_timeout_secs,
                mock_proving_time,
                mock_failure,
                endpoint,
            } => Ok(Self::Mock {
                proof_type: *proof_type,
                proof_timeout: Duration::from_secs(*proof_timeout_secs),
                vm: MockzkVM::new(
                    *proof_type,
                    endpoint.as_ref().map(zkvm_client).transpose()?,
                    mock_proving_time.clone(),
                    *mock_failure,
                ),
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

/// Creates the client of an ere-server.
fn zkvm_client(endpoint: &Url) -> anyhow::Result<zkVMClient> {
    #[cfg(feature = "otel")]
    let middlewares = vec![Box::new(ere_server_client::OtelPropagation) as Box<_>];
    #[cfg(not(feature = "otel"))]
    let middlewares = Vec::new();

    zkVMClient::new(endpoint.clone(), reqwest::Client::new(), middlewares)
        .with_context(|| format!("failed to create zkVM client for endpoint: {endpoint}"))
}

/// Mock zkVM for testing. It proves the expected public values on an ere-server of the mock guest,
/// or sleeps and returns the fixture proof.
#[derive(Debug, Clone)]
pub(crate) struct MockzkVM {
    proof_type: ProofType,
    client: Option<Arc<zkVMClient>>,
    mock_proving_time: MockProvingTime,
    failure: bool,
}

impl MockzkVM {
    /// Constructs a `MockzkVM`.
    pub(crate) fn new(
        proof_type: ProofType,
        client: Option<zkVMClient>,
        mock_proving_time: MockProvingTime,
        failure: bool,
    ) -> Self {
        if let MockProvingTime::Random { min_ms, max_ms, .. } = mock_proving_time {
            assert!(min_ms <= max_ms);
        }
        Self {
            proof_type,
            client: client.map(Arc::new),
            mock_proving_time,
            failure,
        }
    }

    /// Returns the proof of the payload from the ere-server or the fixture.
    pub(crate) async fn prove(&self, input: &StatelessInput) -> anyhow::Result<Vec<u8>> {
        if let Some(client) = &self.client {
            let input = Input::new().with_stdin(input.public_values().to_vec());
            let (_, proof, _) = client.prove(input).await?;
            return Ok(proof.0);
        }

        let start = Instant::now();
        let gas_used = input.payload_meta().gas_used;

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

        Ok(mock_proof(self.proof_type).to_vec())
    }
}

/// Returns the fixture proof of the proof type, a valid proof of another block. Panics for
/// zesu-zisk, whose guest cannot be proved yet.
pub fn mock_proof(proof_type: ProofType) -> &'static [u8] {
    match proof_type {
        ProofType::EthrexOpenVM => {
            include_bytes!("zkvm/mock/stateless-validator-ethrex-openvm-v2.1.0-preview.proof")
        }
        ProofType::EthrexSP1 => {
            include_bytes!("zkvm/mock/stateless-validator-ethrex-sp1-v6.4.0.proof")
        }
        ProofType::EthrexZisk => {
            include_bytes!("zkvm/mock/stateless-validator-ethrex-zisk-v1.1.0-alpha.proof")
        }
        ProofType::RethOpenVM => {
            include_bytes!("zkvm/mock/stateless-validator-reth-openvm-v2.1.0-preview.proof")
        }
        ProofType::RethSP1 => include_bytes!("zkvm/mock/stateless-validator-reth-sp1-v6.4.0.proof"),
        ProofType::RethZisk => {
            include_bytes!("zkvm/mock/stateless-validator-reth-zisk-v1.1.0-alpha.proof")
        }
        ProofType::ZesuZisk => unreachable!("config validation rejects a zesu-zisk mock"),
    }
}

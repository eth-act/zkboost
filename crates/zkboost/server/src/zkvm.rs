//! zkVM instance management and initialization.

use std::sync::Arc;

use anyhow::Context;
use ere_dockerized::DockerizedzkVM;
use ere_server::client::zkVMClient;
use ere_zkvm_interface::{
    Input, ProgramExecutionReport, ProgramProvingReport, Proof, ProofKind, PublicValues,
};
use reqwest::Url;
use zkboost_server_config::zkVMConfig;

use crate::zkvm::mock::MockzkVM;

pub(crate) mod mock;

/// zkVM instance, either dockerized zkVM or external Ere server.
#[allow(non_camel_case_types)]
#[derive(Clone)]
pub(crate) enum zkVMInstance {
    /// Dockerized zkVM managed by zkboost.
    Docker {
        /// The underlying zkVM implementation.
        vm: Arc<DockerizedzkVM>,
    },
    /// External Ere server that provides zkVM functionalities via http endpoints.
    External {
        /// Client of external Ere server.
        client: Arc<zkVMClient>,
    },
    /// Mock zkVM
    Mock(MockzkVM),
}

impl zkVMInstance {
    /// Creates a new zkVM instance from configuration.
    pub(crate) async fn new(config: &zkVMConfig) -> anyhow::Result<Self> {
        match config {
            zkVMConfig::Docker {
                kind,
                resource,
                program,
                ..
            } => {
                let serialized_program = program.load().await?;
                let vm = DockerizedzkVM::new(*kind, serialized_program, resource.clone())
                    .with_context(|| format!("Failed to initialize DockerizedzkVM, kind {kind}"))?;
                Ok(Self::Docker { vm: Arc::new(vm) })
            }
            zkVMConfig::External { endpoint, .. } => {
                let endpoint = Url::parse(endpoint)
                    .with_context(|| format!("Failed to parse endpoint URL: {endpoint}"))?;
                let client = zkVMClient::from_endpoint(endpoint.clone()).with_context(|| {
                    format!("Failed to create zkVM client for endpoint: {endpoint}")
                })?;
                Ok(Self::External {
                    client: Arc::new(client),
                })
            }
            zkVMConfig::Mock {
                mock_proving_time_ms,
                mock_proof_size,
                ..
            } => Ok(Self::Mock(MockzkVM::new(
                *mock_proving_time_ms,
                *mock_proof_size,
            ))),
        }
    }

    /// Executes the program with the given input.
    pub(crate) async fn execute(
        &self,
        input: Input,
    ) -> anyhow::Result<(PublicValues, ProgramExecutionReport)> {
        match self {
            Self::Docker { vm } => vm.execute_async(input).await,
            Self::External { client } => Ok(client.execute(input).await?),
            Self::Mock(vm) => vm.execute(&input).await,
        }
    }

    /// Creates a proof of the program execution with given input.
    pub(crate) async fn prove(
        &self,
        input: Input,
        proof_kind: ProofKind,
    ) -> anyhow::Result<(PublicValues, Proof, ProgramProvingReport)> {
        match self {
            Self::Docker { vm } => vm.prove_async(input, proof_kind).await,
            Self::External { client } => Ok(client.prove(input, proof_kind).await?),
            Self::Mock(vm) => vm.prove(&input, proof_kind).await,
        }
    }

    /// Verifies a proof of the program used to create this zkVM instance, then
    /// returns the public values extracted from the proof.
    pub(crate) async fn verify(&self, proof: Proof) -> anyhow::Result<PublicValues> {
        match self {
            Self::Docker { vm } => vm.verify_async(proof).await,
            Self::External { client } => Ok(client.verify(proof).await?),
            Self::Mock(vm) => vm.verify(&proof).await,
        }
    }
}

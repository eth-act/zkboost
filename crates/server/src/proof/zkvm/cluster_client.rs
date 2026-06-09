//! Clients for external proving clusters, dispatched per zkVM by [`ClusterClient`].

use std::sync::Arc;

use anyhow::Context;
use ere_cluster_client_zisk::{Error as ZiskError, Input, RemoteProverConfig, ZiskClusterClient};
use ere_verifier::zkVMKind;
use ere_verifier_zisk::codec::Encode;
use tracing::warn;
use zkboost_types::ProofType;

/// A client for an external proving cluster, with one variant per supported zkVM.
#[derive(Clone, Debug)]
pub(crate) enum ClusterClient {
    /// A ZisK proving cluster.
    Zisk(Arc<ZiskClusterClient>),
}

impl ClusterClient {
    /// Connects to the cluster at `endpoint` and registers the guest `elf`.
    ///
    /// Returns an error if `proof_type` has no cluster backend.
    pub(crate) async fn new(
        proof_type: ProofType,
        endpoint: &str,
        elf: Vec<u8>,
    ) -> anyhow::Result<Self> {
        let config = RemoteProverConfig {
            endpoint: endpoint.to_string(),
            api_key: None,
        };
        match proof_type.zkvm_kind() {
            zkVMKind::Zisk => {
                let client = ZiskClusterClient::new(&config, elf.into())
                    .await
                    .with_context(|| format!("create zisk cluster client of {endpoint}"))?;
                Ok(Self::Zisk(Arc::new(client)))
            }
            _ => anyhow::bail!("cluster backend does not support {proof_type}"),
        }
    }

    /// Returns the encoded program verifying key.
    pub(crate) fn program_vk(&self) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::Zisk(client) => Ok(client.program_vk().encode_to_vec()?),
        }
    }

    /// Submits a prove job for `input`, returning a [`ClusterProveJob`] that
    /// drives it to completion.
    pub(crate) async fn create_prove_job(&self, input: &Input) -> anyhow::Result<ClusterProveJob> {
        match self {
            Self::Zisk(client) => {
                let job_id = match client.create_prove_job(input).await {
                    Ok(job_id) => job_id,
                    // The cluster was recreated, so rerun setup and resubmit.
                    Err(ZiskError::SetupNotDone) => {
                        client.setup().await.context("rerun zisk cluster setup")?;
                        client
                            .create_prove_job(input)
                            .await
                            .context("resubmit zisk prove job")?
                    }
                    Err(err) => return Err(err).context("submit zisk prove job"),
                };
                Ok(ClusterProveJob::Zisk {
                    client: client.clone(),
                    job_id: Some(job_id),
                })
            }
        }
    }
}

/// A handle to an in-flight cluster prove job.
///
/// Dropping the handle before the job finishes cancels it server-side, so a
/// proof abandoned by [`wait`](Self::wait) does not keep occupying the cluster.
#[derive(Debug)]
pub(crate) enum ClusterProveJob {
    /// An in-flight ZisK prove job.
    Zisk {
        /// Client used to await and cancel the job.
        client: Arc<ZiskClusterClient>,
        /// The job identifier, taken once the job reaches a terminal state.
        job_id: Option<String>,
    },
}

impl ClusterProveJob {
    /// Awaits the job and returns the encoded proof.
    pub(crate) async fn wait(&mut self) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::Zisk { client, job_id } => {
                match client
                    .wait_prove_job(job_id.as_ref().context("job_id not set")?)
                    .await
                {
                    Ok((proof, _)) => {
                        *job_id = None;
                        Ok(proof.encode_to_vec()?)
                    }
                    Err(err @ (ZiskError::JobFailed { .. } | ZiskError::JobCancelled(_))) => {
                        *job_id = None;
                        Err(err)?
                    }
                    Err(err) => Err(err)?,
                }
            }
        }
    }
}

impl Drop for ClusterProveJob {
    fn drop(&mut self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("no runtime to cancel cluster prove job");
            return;
        };
        match self {
            Self::Zisk { client, job_id } => {
                let Some(job_id) = job_id.take() else { return };
                let client = client.clone();
                handle.spawn(async move {
                    if let Err(error) = client.cancel_prove_job(&job_id).await {
                        warn!(%job_id, %error, "failed to cancel zisk cluster prove job");
                    }
                });
            }
        }
    }
}

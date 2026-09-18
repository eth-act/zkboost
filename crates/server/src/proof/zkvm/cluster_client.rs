//! Clients for external proving clusters, dispatched per zkVM by [`ClusterClient`].

use std::sync::Arc;

use anyhow::Context;
use ere_catalog::zkVMKind;
use ere_cluster_client_zisk::{Error as ZiskError, Input, RemoteProverConfig, ZiskClusterClient};
use ere_verifier_zisk::codec::Encode;
use tracing::{Span, info, warn};
use zkboost_types::ProofType;

use crate::proof::zkvm::cluster_client::openvm::{OpenVMClusterClient, OpenVMProveJob};

mod openvm;

/// A client for an external proving cluster, with one variant per supported zkVM.
#[derive(Clone, Debug)]
pub(crate) enum ClusterClient {
    /// A ZisK proving cluster.
    Zisk(Arc<ZiskClusterClient>),
    /// An OpenVM proving cluster.
    OpenVM(Arc<OpenVMClusterClient>),
}

impl ClusterClient {
    /// Connects to the cluster at `endpoint` with the guest `elf`. A ZisK cluster registers the
    /// ELF, an OpenVM cluster must hold it in its loadout already.
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
            zkVMKind::OpenVM => {
                let client = OpenVMClusterClient::new(endpoint, &elf)
                    .await
                    .with_context(|| format!("create openvm cluster client of {endpoint}"))?;
                Ok(Self::OpenVM(Arc::new(client)))
            }
            _ => unreachable!("config validation allows zisk and openvm proof types only"),
        }
    }

    /// Submits a prove job for `input`, returning a [`ClusterProveJob`] that
    /// drives it to completion.
    ///
    /// `prove_span` is the worker's prove span, which declares an empty `job_id` field. The
    /// id of the job is recorded there.
    pub(crate) async fn create_prove_job(
        &self,
        input: &Input,
        prove_span: &Span,
    ) -> anyhow::Result<ClusterProveJob> {
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
                    Err(error) => return Err(error).context("submit zisk prove job"),
                };
                // Recorded on the explicitly passed span rather than `Span::current()`, so the
                // id cannot silently land elsewhere if an intermediate span is ever introduced.
                prove_span.record("job_id", job_id.as_str());
                info!(%job_id, "zisk cluster prove job created");
                Ok(ClusterProveJob::Zisk {
                    client: client.clone(),
                    job_id: Some(job_id),
                })
            }
            Self::OpenVM(client) => Ok(ClusterProveJob::OpenVM(
                client.create_prove_job(input, prove_span).await?,
            )),
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
    /// An in-flight OpenVM prove job, which cancels itself when dropped.
    OpenVM(OpenVMProveJob),
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
                    Err(error @ (ZiskError::JobFailed { .. } | ZiskError::JobCancelled(_))) => {
                        *job_id = None;
                        Err(error)?
                    }
                    Err(error) => Err(error)?,
                }
            }
            Self::OpenVM(job) => job.wait().await,
        }
    }
}

impl Drop for ClusterProveJob {
    fn drop(&mut self) {
        let Self::Zisk { client, job_id } = self else {
            return;
        };
        let Some(job_id) = job_id.take() else { return };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!(%job_id, "no runtime to cancel zisk cluster prove job");
            return;
        };
        let client = client.clone();
        handle.spawn(async move {
            if let Err(error) = client.cancel_prove_job(&job_id).await {
                warn!(%job_id, %error, "failed to cancel zisk cluster prove job");
            }
        });
    }
}

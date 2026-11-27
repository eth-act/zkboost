//! Client of the coordinator of a ZisK proving cluster.

use std::sync::Arc;

use bytes::Bytes;
use ere_cluster_client_zisk::{Error, Input, RemoteProverConfig, ZiskClusterClient};
use ere_codec::Encode;
use tokio_util::task::TaskTracker;
use tracing::Span;

use crate::proof::prover::{CancelOnDrop, Turn};

pub(crate) struct ZiskCluster(Arc<ZiskClusterClient>);

impl ZiskCluster {
    /// Connects to the coordinator with the guest `elf`.
    pub(crate) async fn new(endpoint: &str, elf: Vec<u8>) -> anyhow::Result<Self> {
        let config = RemoteProverConfig {
            endpoint: endpoint.to_owned(),
            api_key: None,
        };
        let client = ZiskClusterClient::new(&config, elf.into()).await?;
        Ok(Self(Arc::new(client)))
    }

    /// Generates the proof of `stdin`, encoded with the codec that the ere verifier reads.
    pub(crate) async fn prove(
        &self,
        stdin: &[u8],
        tasks: &TaskTracker,
        turn: Turn,
    ) -> anyhow::Result<Bytes> {
        let client = &self.0;
        let input = Input::new().with_stdin(stdin.to_vec());
        let job_id = match client.create_prove_job(&input).await {
            // The cluster was recreated, so it needs the setup again.
            Err(Error::SetupNotDone) => {
                client.setup().await?;
                client.create_prove_job(&input).await?
            }
            job_id => job_id?,
        };
        Span::current().record("job_id", job_id.as_str());
        let mut cancel = CancelOnDrop::new(tasks, turn, {
            let (client, job_id) = (client.clone(), job_id.clone());
            async move { Ok(client.cancel_prove_job(&job_id).await.map(|_| ())?) }
        });
        let proof = client.wait_prove_job(&job_id).await;
        if let Ok(_) | Err(Error::JobFailed { .. } | Error::JobCancelled(_)) = proof {
            cancel.disarm();
        }
        Ok(proof?.0.encode_to_vec()?.into())
    }
}

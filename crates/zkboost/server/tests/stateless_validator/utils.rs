use std::{path::PathBuf, process::Command as StdCommand};

use tracing::info;
use zkboost_client::zkboostClient;

pub(crate) struct DockerComposeGuard {
    pub docker_dir: PathBuf,
}

impl Drop for DockerComposeGuard {
    fn drop(&mut self) {
        info!("Stopping docker compose...");
        let _ = StdCommand::new("docker")
            .args(["compose", "down"])
            .current_dir(&self.docker_dir)
            .status();
    }
}

pub(crate) enum ServerResources {
    #[allow(dead_code)]
    Docker(DockerComposeGuard),
    Raw,
}

pub(crate) struct ClientSession {
    pub client: zkboostClient,
    pub _resources: ServerResources,
}

impl std::ops::Deref for ClientSession {
    type Target = zkboostClient;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

use std::{
    path::{Path, PathBuf},
    process::Command as StdCommand,
};

use flate2::read::GzDecoder;
use tar::Archive;
use tokio::fs;
use tracing::info;
use zkboost_client::zkboostClient;
use zkboost_ethereum_el_input::StatelessInput;
use zkboost_ethereum_el_types::{ERE_GUESTS_REPO, ERE_GUESTS_VERSION};

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

pub(crate) async fn fetch_empty_block(workspace: &Path) -> anyhow::Result<StatelessInput> {
    let input_dir = workspace.join("input");
    fs::create_dir_all(&input_dir).await?;

    let fixture_url = format!(
        "https://raw.githubusercontent.com/{ERE_GUESTS_REPO}/{}/crates/integration-tests/fixtures/block.tar.gz",
        ERE_GUESTS_VERSION.as_str(),
    );
    let response = reqwest::get(fixture_url).await?;
    let content = response.bytes().await?;
    let gz = GzDecoder::new(content.as_ref());
    Archive::new(gz).unpack(&input_dir)?;

    let fixture: serde_json::Value =
        serde_json::from_slice(&fs::read(input_dir.join("block/empty_block.json")).await?)?;

    Ok(serde_json::from_value(fixture["stateless_input"].clone())?)
}

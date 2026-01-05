//! Program download utilities for zkVM execution layer binaries.

use std::{path::Path, str::FromStr};

use anyhow::{anyhow, bail};
use ere_common::zkVMKind;
use reqwest::{
    Client, ClientBuilder,
    header::{HeaderName, HeaderValue},
};
use serde::Deserialize;
use tokio::{fs, process::Command};
use tracing::info;
use zkboost_ethereum_el_types::{ERE_GUESTS_REPO, ERE_GUESTS_VERSION, ElKind, PackageVersion};
use zkboost_server_config::{ProgramConfig, UrlConfig};

const ACTION_NAME: &str = "Compile and Release Compiled Guests";

/// Downloads the compiled zkVM program for a specific EL and zkVM combination.
///
/// Programs are retrieved from GitHub, either from release artifacts (for
/// tagged versions) or GitHub Actions artifacts (for git revisions).
///
/// When downloading from Actions artifacts, a GitHub token is required via the
/// `github_token` parameter.
pub async fn download_program(
    el: ElKind,
    zkvm: zkVMKind,
    github_token: Option<&str>,
    output_dir: impl AsRef<Path>,
    always_download: bool,
) -> anyhow::Result<ProgramConfig> {
    let output_dir = output_dir.as_ref();
    fs::create_dir_all(output_dir).await?;

    let artifact_name = format!("stateless-validator-{el}-{zkvm}");
    let artifact_path = output_dir.join(&artifact_name);

    match ERE_GUESTS_VERSION {
        // Download from GitHub releases (no authentication needed)
        PackageVersion::Tag(tag) => {
            if always_download {
                download_release_artifact(tag, &artifact_name, &artifact_path).await?;
            } else {
                return Ok(ProgramConfig::Url(UrlConfig {
                    url: release_artifact_url(tag, &artifact_name),
                }));
            }
        }
        // Download from GitHub Actions artifacts (requires `GITHUB_TOKEN`)
        PackageVersion::Rev(rev) => {
            let github_token = github_token.ok_or_else(|| {
                anyhow!(
                    "`GITHUB_TOKEN` is required when `ere-guests`\
                    dependency uses a git revision instead of a released tag.\
                    Set it via `GITHUB_TOKEN` environment variable."
                )
            })?;
            let gh_client = gh_client(github_token)?;

            let action_id = get_release_action_id(&gh_client, rev).await?;
            let artifact_url = get_artifact_url(&gh_client, &artifact_name, action_id).await?;
            download_action_artifact(&gh_client, &artifact_name, &artifact_url, &artifact_path)
                .await?;
        }
    }

    Ok(ProgramConfig::Path(artifact_path))
}

fn gh_client(github_token: &str) -> anyhow::Result<Client> {
    Ok(ClientBuilder::new()
        .default_headers(
            [
                ("Authorization", format!("Bearer {github_token}").as_str()),
                ("Accept", "application/vnd.github+json"),
                ("X-GitHub-Api-Version", "2022-11-28"),
                ("User-Agent", "zkboost-server"),
            ]
            .into_iter()
            .map(|(name, value)| {
                (
                    HeaderName::from_str(name).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect(),
        )
        .build()?)
}

async fn get_release_action_id(gh_client: &Client, rev: &str) -> anyhow::Result<u64> {
    info!("Getting release action id of workload repo at {rev}...");

    #[derive(Deserialize)]
    struct WorkflowRunsResponse {
        workflow_runs: Vec<WorkflowRun>,
    }

    #[derive(Deserialize)]
    struct WorkflowRun {
        id: u64,
        name: String,
        status: String,
        conclusion: Option<String>,
    }

    let url = format!("https://api.github.com/repos/{ERE_GUESTS_REPO}/actions/runs?head_sha={rev}");
    let res: WorkflowRunsResponse = gh_client.get(&url).send().await?.json().await?;
    res.workflow_runs
        .into_iter()
        .filter(|run| {
            run.name == ACTION_NAME
                && run.status == "completed"
                && run.conclusion.as_deref() == Some("success")
        })
        .max_by_key(|run| run.id)
        .map(|run| run.id)
        .ok_or_else(|| anyhow!("No successful '{ACTION_NAME}' workflow run found for commit {rev}"))
}

async fn get_artifact_url(
    gh_client: &Client,
    artifact_name: &str,
    action_id: u64,
) -> anyhow::Result<String> {
    info!("Getting artifact url of artifact {artifact_name} of action id {action_id}...");

    #[derive(Deserialize)]
    struct ArtifactsResponse {
        artifacts: Vec<Artifact>,
    }

    #[derive(Deserialize)]
    struct Artifact {
        name: String,
        archive_download_url: String,
    }

    let url = format!(
        "https://api.github.com/repos/{ERE_GUESTS_REPO}/actions/runs/{action_id}/artifacts"
    );
    let res: ArtifactsResponse = gh_client.get(&url).send().await?.json().await?;
    res.artifacts
        .into_iter()
        .find_map(|artifact| {
            (artifact.name == artifact_name).then_some(artifact.archive_download_url)
        })
        .ok_or_else(|| anyhow!("Artifact '{artifact_name}' not found in action run {action_id}"))
}

async fn download_action_artifact(
    gh_client: &Client,
    artifact_name: &str,
    artifact_url: &str,
    artifact_path: &Path,
) -> anyhow::Result<()> {
    info!(
        "Downloading artifact {artifact_name} from {artifact_url} into {}...",
        artifact_path.display(),
    );

    let artifact_zip_path = artifact_path.with_extension("zip");
    let res = gh_client.get(artifact_url).send().await?;
    fs::write(&artifact_zip_path, res.bytes().await?).await?;

    let status = Command::new("unzip")
        .arg("-o")
        .arg(artifact_zip_path.canonicalize()?)
        .current_dir(artifact_path.parent().unwrap().canonicalize()?)
        .status()
        .await?;
    if !status.success() {
        bail!("Failed to unzip artifact");
    }

    if !fs::try_exists(artifact_path).await? {
        bail!(
            "Artifact file not found at '{}' after unzipping",
            artifact_path.display(),
        );
    }

    fs::remove_file(&artifact_zip_path).await?;

    Ok(())
}

fn release_artifact_url(tag: &str, artifact_name: &str) -> String {
    format!("https://github.com/{ERE_GUESTS_REPO}/releases/download/{tag}/{artifact_name}")
}

async fn download_release_artifact(
    tag: &str,
    artifact_name: &str,
    artifact_path: &Path,
) -> anyhow::Result<()> {
    let artifact_url = release_artifact_url(tag, artifact_name);

    info!(
        "Downloading artifact {artifact_name} from {artifact_url} into {}...",
        artifact_path.display(),
    );

    let res = reqwest::get(artifact_url).await?;
    fs::write(artifact_path, res.bytes().await?).await?;

    Ok(())
}

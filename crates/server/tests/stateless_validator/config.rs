use std::{path::Path, str::FromStr};

use anyhow::{anyhow, bail};
use axum::http::{HeaderName, HeaderValue};
use reqwest::{Client, ClientBuilder};
use serde::Deserialize;
use tokio::{fs, process::Command};
use tracing::info;

use crate::{
    Args,
    util::{PackageVersion, WORKLOAD_PKG_VERSION, WORKLOAD_REPO},
};

const RELEASE_ACTION_NAME: &str = "Release Compiled Guests";

pub(crate) async fn generate_config(args: &Args, workspace: &Path) -> anyhow::Result<String> {
    info!("Generating config...");

    let program = download_program(args, workspace).await?;

    let config_path = workspace.join("config.toml");
    let config = format!(
        r#"[[zkvm]]
kind = "{}"
resource = "{}"
program-id = "{}"
program = {program}
"#,
        args.zkvm,
        args.resource,
        args.program_id(),
    );
    fs::write(&config_path, config).await?;

    info!("Successfully generated config");

    Ok(config_path.to_string_lossy().to_string())
}

async fn download_program(args: &Args, workspace: &Path) -> anyhow::Result<String> {
    let workload_repo = &*WORKLOAD_REPO;
    let program_id = args.program_id();

    match &*WORKLOAD_PKG_VERSION {
        // Download from GitHub releases (no authentication needed)
        PackageVersion::Tag(tag) => Ok(format!(
            r#"{{ url = "https://github.com/{workload_repo}/releases/download/{tag}/{program_id}" }}"#,
        )),
        // Download from GitHub Actions artifacts (requires `GITHUB_TOKEN`)
        PackageVersion::Rev(rev) => {
            let github_token = args
                .github_token
                .as_deref()
                .ok_or_else(|| anyhow!("`GITHUB_TOKEN` is required when `benchmark-runner` dependency uses a git revision instead of a released tag. Set it via `GITHUB_TOKEN` environment variable."))?;

            let gh_client = gh_client(github_token)?;

            let action_id = get_release_action_id(&gh_client, rev).await?;
            let artifact_url = get_artifact_url(&gh_client, &program_id, action_id).await?;
            let artifact_path =
                download_artifact(&gh_client, &program_id, &artifact_url, workspace).await?;

            Ok(format!(r#"{{ path = "{artifact_path}" }}"#))
        }
    }
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

async fn get_release_action_id(client: &Client, rev: &str) -> anyhow::Result<u64> {
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

    let workload_repo = &*WORKLOAD_REPO;
    let url = format!("https://api.github.com/repos/{workload_repo}/actions/runs?head_sha={rev}");
    let res: WorkflowRunsResponse = client.get(&url).send().await?.json().await?;
    res.workflow_runs
        .into_iter()
        .filter(|run| {
            run.name == RELEASE_ACTION_NAME
                && run.status == "completed"
                && run.conclusion.as_deref() == Some("success")
        })
        .max_by_key(|run| run.id)
        .map(|run| run.id)
        .ok_or_else(|| {
            anyhow!("No successful '{RELEASE_ACTION_NAME}' workflow run found for commit {rev}")
        })
}

async fn get_artifact_url(
    client: &Client,
    program_id: &str,
    action_id: u64,
) -> anyhow::Result<String> {
    info!("Getting artifact url of artifact {program_id} of action id {action_id}...");

    #[derive(Deserialize)]
    struct ArtifactsResponse {
        artifacts: Vec<Artifact>,
    }

    #[derive(Deserialize)]
    struct Artifact {
        name: String,
        archive_download_url: String,
    }

    let workload_repo = &*WORKLOAD_REPO;
    let url =
        format!("https://api.github.com/repos/{workload_repo}/actions/runs/{action_id}/artifacts");
    let res: ArtifactsResponse = client.get(&url).send().await?.json().await?;
    res.artifacts
        .into_iter()
        .find_map(|artifact| (artifact.name == program_id).then_some(artifact.archive_download_url))
        .ok_or_else(|| anyhow!("Artifact '{program_id}' not found in action run {action_id}"))
}

async fn download_artifact(
    client: &Client,
    program_id: &str,
    artifact_url: &str,
    workspace: &Path,
) -> anyhow::Result<String> {
    info!("Downloading artifact {program_id} from {artifact_url}...");

    let program_dir = workspace.join("program");
    fs::create_dir_all(&program_dir).await?;

    let artifact_zip_path = program_dir.join(format!("{program_id}.zip"));
    let res = client.get(artifact_url).send().await?;
    fs::write(&artifact_zip_path, res.bytes().await?).await?;

    let status = Command::new("unzip")
        .arg("-o")
        .arg(&artifact_zip_path)
        .current_dir(&program_dir)
        .status()
        .await?;
    if !status.success() {
        bail!("Failed to unzip artifact");
    }

    let artifact_path = program_dir.join(program_id);
    if !fs::try_exists(&artifact_path).await? {
        bail!(
            "Artifact file not found at '{}' after unzipping",
            artifact_path.display(),
        );
    }

    Ok(artifact_path.to_string_lossy().to_string())
}

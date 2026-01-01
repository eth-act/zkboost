//! Build script that generates workload repository and version information.

use std::{env, fs, path::Path};

fn main() {
    generate_workload_info();
    println!("cargo:rerun-if-changed=Cargo.lock");
}

fn generate_workload_info() {
    let metadata = cargo_metadata::MetadataCommand::new().exec().unwrap();

    // Repo in format of `{org}/{repo}` e.g. `eth-act/zkevm-benchmark-workload`.
    let workload_repo = metadata
        .packages
        .iter()
        .find(|pkg| pkg.name == "benchmark-runner")
        .and_then(|pkg| {
            let url = &pkg.source.as_ref()?.repr;
            let repo = url.strip_prefix("git+https://github.com/")?;
            let (repo, _) = repo.split_once('?')?;
            Some(format!(r#""{repo}""#))
        })
        .unwrap_or_else(|| panic!("Failed to find repo of `benchmark-runner`"));

    let workload_version = metadata
        .packages
        .iter()
        .find(|pkg| pkg.name == "benchmark-runner")
        .and_then(|pkg| {
            let url = &pkg.source.as_ref()?.repr;
            let (_, query) = url.split_once('?')?;

            if let Some(query) = query.strip_prefix("tag=") {
                let (tag, _) = query.split_once('#')?;
                Some(format!(r#"PackageVersion::Tag("{tag}")"#))
            } else {
                let (_, rev) = query.split_once('#')?;
                Some(format!(r#"PackageVersion::Rev("{rev}")"#))
            }
        })
        .unwrap_or_else(|| panic!("Failed to find version of `benchmark-runner`"));

    let path = Path::new(&env::var("OUT_DIR").unwrap()).join("workload_info.rs");
    let content = format!(
        r#"/// Workload repo in format of `{{org}}/{{repo}}` e.g. `eth-act/zkevm-benchmark-workload`.
pub const WORKLOAD_REPO: &str = {workload_repo};
/// Workload version in tag or revision.
pub const WORKLOAD_VERSION: PackageVersion = {workload_version};"#
    );
    fs::write(path, content).unwrap();
}

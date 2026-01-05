//! Build script that generates `ere-guests` repository and version information.

use std::{env, fs, path::Path};

fn main() {
    generate_ere_guests_info();
    println!("cargo:rerun-if-changed=Cargo.lock");
}

fn generate_ere_guests_info() {
    let metadata = cargo_metadata::MetadataCommand::new().exec().unwrap();

    let url = metadata.packages.iter().find_map(|pkg| {
        let url = &pkg.source.as_ref()?.repr;
        url.contains("ere-guests").then_some(url)
    });

    // Repo in format of `{org}/{repo}` e.g. `eth-act/ere-guests`.
    let repo = url
        .and_then(|url| {
            let repo = url.strip_prefix("git+https://github.com/")?;
            let (repo, _) = repo.split_once('?')?;
            Some(format!(r#""{repo}""#))
        })
        .unwrap_or_else(|| panic!("Failed to parse repo from `{url:?}`"));

    let version = url
        .and_then(|url| {
            let (_, query) = url.split_once('?')?;

            if let Some(query) = query.strip_prefix("tag=") {
                let (tag, _) = query.split_once('#')?;
                Some(format!(r#"PackageVersion::Tag("{tag}")"#))
            } else {
                let (_, rev) = query.split_once('#')?;
                Some(format!(r#"PackageVersion::Rev("{rev}")"#))
            }
        })
        .unwrap_or_else(|| panic!("Failed to parse version from `{url:?}`"));

    let path = Path::new(&env::var("OUT_DIR").unwrap()).join("workload_info.rs");
    let content = format!(
        r#"/// Ere guests repo in format of `{{org}}/{{repo}}` e.g. `eth-act/ere-guests`.
pub const ERE_GUESTS_REPO: &str = {repo};
/// Ere guests version in tag or revision.
pub const ERE_GUESTS_VERSION: PackageVersion = {version};"#
    );
    fs::write(path, content).unwrap();
}

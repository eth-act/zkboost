//! Build script that generates `ere-guests` repository and version information.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    generate_ere_guests_info();

    if let Some(cargo_lock_path) = cargo_lock_path() {
        println!("cargo:rerun-if-changed={}", cargo_lock_path.display());
    }
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

/// Returns path to the closest workspace that contains `Cargo.lock` from `CARGO_MANIFEST_DIR`,
/// returns `None` if not found.
pub fn workspace() -> Option<PathBuf> {
    let mut dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .canonicalize()
        .ok()?;
    loop {
        if dir.join("Cargo.lock").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Returns path to the closest `Cargo.lock` from `CARGO_MANIFEST_DIR`, returns `None` if not found.
pub fn cargo_lock_path() -> Option<PathBuf> {
    workspace().map(|workspace| workspace.join("Cargo.lock"))
}

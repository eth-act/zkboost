use std::sync::LazyLock;

/// Workload repo in format of `{org}/{repo}` e.g. `eth-act/zkevm-benchmark-workload`.
pub(crate) static WORKLOAD_REPO: LazyLock<String> = LazyLock::new(|| {
    let metadata = cargo_metadata::MetadataCommand::new().exec().unwrap();
    metadata
        .packages
        .iter()
        .find(|pkg| pkg.name == "benchmark-runner")
        .and_then(|pkg| {
            let url = &pkg.source.as_ref()?.repr;
            let repo = url.strip_prefix("git+https://github.com/")?;
            let (repo, _) = repo.split_once('?')?;
            Some(repo.to_string())
        })
        .unwrap_or_else(|| panic!("Failed to find repo of `benchmark-runner`"))
});

pub(crate) static WORKLOAD_PKG_VERSION: LazyLock<PackageVersion> = LazyLock::new(|| {
    let metadata = cargo_metadata::MetadataCommand::new().exec().unwrap();
    metadata
        .packages
        .iter()
        .find(|pkg| pkg.name == "benchmark-runner")
        .and_then(|pkg| {
            let url = &pkg.source.as_ref()?.repr;
            let (_, query) = url.split_once('?')?;

            if let Some(query) = query.strip_prefix("tag=") {
                let (tag, _) = query.split_once('#')?;
                Some(PackageVersion::Tag(tag.to_string()))
            } else {
                let (_, rev) = query.split_once('#')?;
                Some(PackageVersion::Rev(rev.to_string()))
            }
        })
        .unwrap_or_else(|| panic!("Failed to find version of `benchmark-runner`"))
});

pub(crate) enum PackageVersion {
    Rev(String),
    Tag(String),
}

impl PackageVersion {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Rev(s) | Self::Tag(s) => s.as_str(),
        }
    }
}

//! Type definitions for zkBoost Ethereum Execution Layer integration.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub use el_kind::ElKind;

pub mod el_kind;

include!(concat!(env!("OUT_DIR"), "/workload_info.rs"));

/// Version specification for a package, either as a git revision or release tag.
#[derive(Debug)]
pub enum PackageVersion {
    /// Git commit revision (SHA).
    Rev(&'static str),
    /// Release tag version.
    Tag(&'static str),
}

impl PackageVersion {
    /// Returns the version string, whether it's a revision or tag.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rev(s) | Self::Tag(s) => s,
        }
    }
}

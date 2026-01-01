//! Server configuration and TOML/YAML parsing.
//!
//! Defines the configuration structure for loading zkVM programs from TOML/YAML files.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod config;

pub use config::{Config, PathConfig, ProgramConfig, UrlConfig, zkVMConfig};

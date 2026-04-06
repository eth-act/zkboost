//! zkboost proof node library.
//!
//! Re-exports internal modules so that integration tests and the binary
//! can share the same code.

pub mod config;
pub(crate) mod dashboard;
pub mod el_client;
pub mod http;
pub mod metrics;
pub mod proof;
pub mod server;
pub mod witness;

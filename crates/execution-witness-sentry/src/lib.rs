//! Execution witness sentry - monitors execution layer nodes for new blocks
//! and fetches their execution witnesses.
//!
//! This crate provides functionality to:
//! - Subscribe to new block headers via WebSocket
//! - Fetch blocks and execution witnesses via JSON-RPC
//! - Store block data and witnesses to disk
//! - Submit execution proofs to consensus layer nodes
//!
//! ## Example
//!
//! ```ignore
//! use execution_witness_sentry::{Config, ElClient, subscribe_blocks};
//!
//! let config = Config::load("config.toml")?;
//! let client = ElClient::new(url);
//!
//! // Subscribe to new blocks
//! let mut stream = subscribe_blocks(&ws_url).await?;
//!
//! while let Some(header) = stream.next().await {
//!     let witness = client.get_execution_witness(header.number).await?;
//!     // Process witness...
//! }
//! ```

pub mod cl_subscription;
pub mod config;
pub mod error;
pub mod rpc;
pub mod storage;
pub mod subscription;

// Re-export main types at crate root for convenience.
pub use cl_subscription::{BlockEvent, ClEvent, ClEventStream, HeadEvent, subscribe_cl_events};
pub use config::{ClEndpoint, Config, Endpoint};
pub use error::{Error, Result};
pub use rpc::{BlockInfo, ClClient, ElClient, ExecutionProof, generate_random_proof};
pub use storage::{
    BlockMetadata, BlockStorage, SavedProof, compress_gzip, decompress_gzip, load_block_data,
};
pub use subscription::subscribe_blocks;

// Re-export alloy types that appear in our public API.
pub use alloy_rpc_types_eth::{Block, Header};

//! HTTP client for zkboost servers.
//!
//! Provides a high-level Rust client for interacting with zkboost servers to execute zkVM
//! programs, generate proofs, and verify proofs over HTTP.
//!
//! ## Example
//!
//! ```no_run
//! use zkboost_client::{zkboostClient, Error};
//!
//! # async fn example() -> Result<(), Error> {
//! let client = zkboostClient::new("http://localhost:3001")?;
//!
//! // Execute a program
//! let response = client.execute("my_program", vec![1, 2, 3, 4]).await?;
//! println!("Execution completed in {} cycles", response.total_num_cycles);
//!
//! // Request for proof generation
//! let proof_response = client.prove("my_program", vec![1, 2, 3, 4]).await?;
//!
//! // Wait for proof from webhook
//! let proof = Vec::new();
//!
//! // Verify the proof
//! let verify_response = client.verify("my_program", proof).await?;
//! assert!(verify_response.verified);
//! # Ok(())
//! # }
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub use client::zkboostClient;
pub use error::Error;
pub use zkboost_types as types;

mod client;
mod error;

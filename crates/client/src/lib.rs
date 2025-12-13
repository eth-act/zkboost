//! HTTP client for zkboost servers.
//!
//! Provides a high-level Rust client for interacting with zkboost servers to execute zkVM
//! programs, generate proofs, and verify proofs over HTTP.
//!
//! ## Example
//!
//! ```no_run
//! use zkboost_client::zkBoostClient;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let client = zkBoostClient::new("http://localhost:3001");
//!
//! // Execute a program
//! let response = client.execute("my_program", vec![1, 2, 3, 4]).await?;
//! println!("Execution completed in {} cycles", response.total_num_cycles);
//!
//! // Generate a proof
//! let proof_response = client.prove("my_program", vec![1, 2, 3, 4]).await?;
//!
//! // Verify the proof
//! let verify_response = client.verify("my_program", proof_response.proof).await?;
//! assert!(verify_response.verified);
//! # Ok(())
//! # }
//! ```

pub use client::zkBoostClient;
pub use zkboost_types as types;

mod client;

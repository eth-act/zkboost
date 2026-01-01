//! Ethereum Execution Layer relayer for zkboost proof generation.
//!
//! This relayer orchestrates the complete workflow for generating proofs of
//! EL stateless validation:
//!
//! 1. Listen to new block from EL
//! 2. Fetch execution witness from EL
//! 3. Generate input for EL stateless validator guest program
//! 4. Request zkboost-server for proof
//! 5. Send proof back to CL
//!
//! ## Architecture
//!
//! ```text
//!   CL          Relayer               EL           zkboost-server
//!   |              |                  |                  |
//!   |              |<----new block----|                  |
//!   |              |                  |                  |
//!   |              |--fetch witness-->|                  |
//!   |              |<----witness------|                  |
//!   |              |                  |                  |
//!   |    (generate zkVM input)        |                  |
//!   |              |                  |                  |
//!   |              |--request proof--------------------->|
//!   |              |<------proof-------------------------|
//!   |              |                  |                  |
//!   |<----proof----|                  |                  |
//!   |              |                  |                  |
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

fn main() {}

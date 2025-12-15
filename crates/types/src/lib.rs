//! Shared type definitions for zkboost.
//!
//! This crate provides the request and response types used by both the zkboost server
//! and client for zkVM execution, proving, and verification operations.
//!
//! ## Overview
//!
//! The types are organized around 4 main operations:
//! - Execute - Run zkVM programs without generating proofs
//! - Prove - Generate cryptographic proofs of program execution
//! - Verify - Verify proofs without re-execution
//! - Info - Query server hardware and system information
//!
//! All binary data (inputs, outputs, proofs) is serialized as base64 when transmitted over HTTP.

pub use ere_zkvm_interface::PublicValues;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_with::{base64::Base64, serde_as};

/// Unique identifier for a zkVM program.
///
/// This is a wrapper around a `String` that provides type safety for program identifiers.
/// Programs are identified by their unique ID when making requests to execute, prove, or verify.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Hash)]
#[serde(transparent)]
pub struct ProgramID(pub String);

impl From<String> for ProgramID {
    fn from(s: String) -> Self {
        ProgramID(s)
    }
}

impl From<&str> for ProgramID {
    fn from(s: &str) -> Self {
        ProgramID(s.to_string())
    }
}

/// Request to execute a zkVM program.
///
/// This initiates program execution without generating a proof. Execution is faster than
/// proving and is useful for testing program logic and obtaining execution metrics.
#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecuteRequest {
    /// The unique identifier of the program to execute.
    pub program_id: ProgramID,
    /// The input data for the program, encoded as base64 when serialized.
    #[serde_as(as = "Base64")]
    pub input: Vec<u8>,
}

/// Response from executing a zkVM program.
///
/// Contains the execution results including public outputs and performance metrics.
#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecuteResponse {
    /// The unique identifier of the executed program.
    pub program_id: ProgramID,
    /// The public values output by the program, encoded as base64 when serialized.
    #[serde_as(as = "Base64")]
    pub public_values: PublicValues,
    /// Total number of cycles for the entire execution.
    pub total_num_cycles: u64,
    /// Region-specific cycle counts, mapping region names to their cycle counts.
    pub region_cycles: IndexMap<String, u64>,
    /// Execution time in milliseconds.
    pub execution_time_ms: u128,
}

/// Request to generate a proof for a zkVM program execution.
///
/// This runs the program and generates a cryptographic proof that the execution
/// was performed correctly. The proof can later be verified without re-executing the program.
#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct ProveRequest {
    /// The unique identifier of the program to prove.
    pub program_id: ProgramID,
    /// The input data for the program, encoded as base64 when serialized.
    #[serde_as(as = "Base64")]
    pub input: Vec<u8>,
}

/// Response from generating a proof for a zkVM program execution.
///
/// Contains the generated proof along with the public outputs and performance metrics.
#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct ProveResponse {
    /// The unique identifier of the proved program.
    pub program_id: ProgramID,
    /// The public values output by the program, encoded as base64 when serialized.
    #[serde_as(as = "Base64")]
    pub public_values: PublicValues,
    /// The generated cryptographic proof, encoded as base64 when serialized.
    #[serde_as(as = "Base64")]
    pub proof: Vec<u8>,
    /// Proving time in milliseconds.
    pub proving_time_ms: u128,
}

/// Request to verify a proof.
///
/// Verifies that a proof is valid for the specified program without re-executing it.
/// Verification is much faster than proof generation.
#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct VerifyRequest {
    /// The unique identifier of the program that generated the proof.
    pub program_id: ProgramID,
    /// The proof to verify, encoded as base64 when serialized.
    #[serde_as(as = "Base64")]
    pub proof: Vec<u8>,
}

/// Response from verifying a proof.
///
/// Contains the verification result and extracted public values if successful.
#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct VerifyResponse {
    /// The unique identifier of the program.
    pub program_id: ProgramID,
    /// The public values extracted from the proof, encoded as base64 when serialized.
    #[serde_as(as = "Base64")]
    pub public_values: PublicValues,
    /// Whether the proof verification succeeded.
    pub verified: bool,
    /// Human-readable reason for verification failure. Empty if `verified` is true.
    pub failure_reason: String,
}

/// Information about the server's hardware and operating system.
///
/// Provides details about the compute environment where zkVM programs are executed.
#[derive(Debug, Serialize, Deserialize)]
pub struct ServerInfoResponse {
    /// CPU information.
    pub cpu: CpuInfo,
    /// Memory information.
    pub memory: MemoryInfo,
    /// Operating system information.
    pub os: OsInfo,
    /// CPU architecture (e.g., "x86_64", "aarch64").
    pub architecture: String,
    /// GPU information, or "No GPU detected" if none available.
    pub gpu: String,
}

/// CPU information for the server.
#[derive(Debug, Serialize, Deserialize)]
pub struct CpuInfo {
    /// CPU model name.
    pub model: String,
    /// Number of physical CPU cores.
    pub cores: usize,
    /// CPU frequency in MHz.
    pub frequency: u64,
    /// CPU vendor (e.g., "GenuineIntel", "AuthenticAMD").
    pub vendor: String,
}

/// Memory information for the server.
#[derive(Debug, Serialize, Deserialize)]
pub struct MemoryInfo {
    /// Total system memory (formatted as human-readable string, e.g., "16.00 GB").
    pub total: String,
    /// Available system memory (formatted as human-readable string).
    pub available: String,
    /// Used system memory (formatted as human-readable string).
    pub used: String,
}

/// Operating system information for the server.
#[derive(Debug, Serialize, Deserialize)]
pub struct OsInfo {
    /// Operating system name (e.g., "Linux", "macOS").
    pub name: String,
    /// Operating system version.
    pub version: String,
    /// Kernel version.
    pub kernel: String,
}

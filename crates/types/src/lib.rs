//! Shared request/response types for the zkboost Proof Node API.
//!
//! This crate contains the types used by both the zkboost server and client
//! for REST API communication and SSE event streaming.

#![warn(unused_crate_dependencies)]

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
};

pub use ere_guests_stateless_validator_common::{
    HashTreeRoot, Sha2Hasher, SszDecode, SszEncode, SszList, SszVector,
    guest::input::{
        BlobSchedule, ChainConfig, ExecutionWitness, ForkActivation, ForkConfig, ProtocolFork,
        new_payload_request::*,
    },
};
use libssz_derive::{SszDecode, SszEncode};
pub use proof_type::*;
use serde::{Deserialize, Serialize};

mod proof_type;

/// 32-bytes Hash.
pub type Hash256 = alloy_primitives::B256;

/// SSZ-encoded request body for `POST /v1/execution_proof_requests`.
#[derive(Debug, Clone, Eq, PartialEq, SszEncode, SszDecode)]
pub struct ProofRequestBody {
    /// The payload to prove.
    pub new_payload_request: NewPayloadRequest,
    /// Expected chain config to prove the payload against (resolved active fork).
    pub chain_config: ChainConfig,
    /// Proof types to generate for this payload.
    pub proof_types: Vec<ProofType>,
}

/// Response for `POST /v1/execution_proof_requests`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProofRequestResponse {
    /// The tree-hash root of the `NewPayloadRequest` used as the identifier.
    pub new_payload_request_root: Hash256,
}

/// Query params for `GET /v1/execution_proof_requests` (SSE).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProofEventQuery {
    /// Optional filter to get stream events only for this root.
    pub new_payload_request_root: Option<Hash256>,
}

/// SSZ-encoded request body for `POST /v1/execution_proof_verifications`.
#[derive(Debug, Clone, Eq, PartialEq, SszEncode, SszDecode)]
pub struct ProofVerificationBody {
    /// The root identifying the proven payload request.
    pub new_payload_request_root: [u8; 32],
    /// Expected chain config to verify the proof against (resolved active fork).
    pub chain_config: ChainConfig,
    /// The proof type being verified.
    pub proof_type: ProofType,
    /// The proof bytes.
    pub proof: Vec<u8>,
}

/// Response for `POST /v1/execution_proof_verifications`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProofVerificationResponse {
    /// The verification result.
    pub status: ProofStatus,
}

/// Verification status returned by the proof verification endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProofStatus {
    /// The proof is valid.
    Valid,
    /// The proof is invalid.
    Invalid,
}

/// Response for `GET /v1/proof_types`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProofTypesResponse {
    /// List of initialized proof types with their capabilities.
    pub proof_types: Vec<ProofTypeInfo>,
}

/// Information about a single initialized proof type.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProofTypeInfo {
    /// The proof type identifier (e.g., "reth-zisk").
    pub proof_type: ProofType,
    /// The backend kind: "ere", "mock", or "verifier".
    pub kind: BackendKind,
    /// Whether this backend can generate proofs.
    pub can_prove: bool,
    /// Whether this backend can verify proofs.
    pub can_verify: bool,
}

/// Backend kind for a zkVM instance.
///
/// Uses the same terminology as zkboost configuration.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Remote ere-server backend.
    Ere,
    /// In-process mock backend for testing.
    Mock,
    /// In-process verifier-only backend.
    Verifier,
    /// Remote cluster backend.
    Cluster,
}

impl ProofStatus {
    /// Returns `true` if proof status is `ProofStatus::Valid`:
    pub fn is_valid(&self) -> bool {
        *self == Self::Valid
    }
}

/// SSE event types broadcast to HTTP SSE subscribers.
#[derive(Debug, Clone, Eq, PartialEq, strum::EnumDiscriminants)]
#[strum_discriminants(name(ProofEventKind))]
#[strum_discriminants(derive(Hash))]
#[strum_discriminants(doc = "Discriminant enum for [`ProofEvent`] variants.")]
pub enum ProofEvent {
    /// A proof completed successfully.
    ProofComplete(ProofComplete),
    /// A proof failed.
    ProofFailure(ProofFailure),
}

impl ProofEvent {
    /// Returns the discriminant kind for this event.
    pub fn kind(&self) -> ProofEventKind {
        ProofEventKind::from(self)
    }

    /// Returns the `new_payload_request_root` from the event.
    pub fn new_payload_request_root(&self) -> Hash256 {
        match self {
            Self::ProofComplete(inner) => inner.new_payload_request_root,
            Self::ProofFailure(inner) => inner.new_payload_request_root,
        }
    }

    /// Returns the [`ProofType`] from the event.
    pub fn proof_type(&self) -> ProofType {
        match self {
            Self::ProofComplete(inner) => inner.proof_type,
            Self::ProofFailure(inner) => inner.proof_type,
        }
    }

    /// Returns the SSE event name for this variant.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::ProofComplete(_) => "proof_complete",
            Self::ProofFailure(_) => "proof_failure",
        }
    }

    /// Serializes the inner payload to a JSON string.
    pub fn to_parts(&self) -> (&'static str, String) {
        let data = match self {
            Self::ProofComplete(inner) => serde_json::to_string(inner),
            Self::ProofFailure(inner) => serde_json::to_string(inner),
        }
        .expect("ProofEvent serialization is infallible");
        (self.event_name(), data)
    }

    /// Reconstructs a [`ProofEvent`] from an SSE event name and JSON data.
    pub fn try_from_parts(name: &str, data: &str) -> Result<Self, ProofEventParseError> {
        match name {
            "proof_complete" => Ok(Self::ProofComplete(serde_json::from_str(data)?)),
            "proof_failure" => Ok(Self::ProofFailure(serde_json::from_str(data)?)),
            other => Err(ProofEventParseError::UnknownEvent(other.to_string())),
        }
    }
}

impl From<ProofComplete> for ProofEvent {
    fn from(inner: ProofComplete) -> Self {
        Self::ProofComplete(inner)
    }
}

impl From<ProofFailure> for ProofEvent {
    fn from(inner: ProofFailure) -> Self {
        Self::ProofFailure(inner)
    }
}

/// Error returned when parsing an SSE event into a [`ProofEvent`] fails.
#[derive(Debug)]
pub enum ProofEventParseError {
    /// JSON deserialization failed.
    Json(serde_json::Error),
    /// The event name does not match any known variant.
    UnknownEvent(String),
}

impl Display for ProofEventParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(e) => write!(f, "JSON parse error: {e}"),
            Self::UnknownEvent(name) => write!(f, "unknown SSE event type: {name}"),
        }
    }
}

impl Error for ProofEventParseError {}

impl From<serde_json::Error> for ProofEventParseError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// Payload for a successful proof event.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProofComplete {
    /// Beacon-level identifier for this payload.
    pub new_payload_request_root: Hash256,
    /// Proof type.
    pub proof_type: ProofType,
    /// Time the request waited for its execution witness, in milliseconds
    /// (request admission until the witness became available, including fetch
    /// retries and coalesced waits behind an earlier fetch for the same block).
    ///
    /// `None` when the producer predates this field or replays a cached proof
    /// whose timings are no longer known — absence means "unknown", not zero.
    #[serde(default)]
    pub witness_ms: Option<u64>,
    /// Time the proof job waited in the worker queue between dispatch and
    /// dequeue, in milliseconds. `None` semantics as for `witness_ms`.
    #[serde(default)]
    pub queue_wait_ms: Option<u64>,
    /// Proof generation time after dequeue, in milliseconds. `None` semantics
    /// as for `witness_ms`.
    #[serde(default)]
    pub prove_ms: Option<u64>,
}

/// Payload for a failed proof event.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProofFailure {
    /// Beacon-level identifier for this payload.
    pub new_payload_request_root: Hash256,
    /// Proof type.
    pub proof_type: ProofType,
    /// Structured reason for the failure.
    pub reason: FailureReason,
    /// Human-readable error message with details about the failure.
    pub error: String,
}

/// Failure reason of a proof request.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    /// The execution witness could not be fetched within the configured timeout.
    WitnessTimeout,
    /// Proof generation did not complete within the configured timeout.
    ProvingTimeout,
    /// A general error occurred during proving.
    ProvingError,
    /// An internal error occurred.
    InternalError,
}

#[cfg(test)]
mod tests {
    use crate::{
        BackendKind, Hash256, ProofComplete, ProofType, ProofTypeInfo, ProofTypesResponse,
    };

    /// A ProofComplete emitted by a producer that predates the stage-timing
    /// fields must still deserialize — absent keys read as None, not an error.
    #[test]
    fn test_proof_complete_deserializes_without_stage_timings() {
        let legacy = r#"{
            "new_payload_request_root": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "proof_type": "reth-zisk"
        }"#;
        let event: ProofComplete = serde_json::from_str(legacy).unwrap();
        assert_eq!(event.witness_ms, None);
        assert_eq!(event.queue_wait_ms, None);
        assert_eq!(event.prove_ms, None);
    }

    /// A consumer built before new fields exist must tolerate them: serde's
    /// default behavior ignores unknown keys, and nothing here may opt out of
    /// that (no deny_unknown_fields), or producers could never add fields.
    #[test]
    fn test_proof_complete_tolerates_unknown_fields() {
        let future = r#"{
            "new_payload_request_root": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "proof_type": "reth-zisk",
            "some_future_field": 42
        }"#;
        let event: ProofComplete = serde_json::from_str(future).unwrap();
        assert_eq!(event.proof_type, ProofType::RethZisk);
    }

    /// Populated stage timings survive a serialize/deserialize round trip.
    #[test]
    fn test_proof_complete_stage_timings_round_trip() {
        let event = ProofComplete {
            new_payload_request_root: Hash256::from([1u8; 32]),
            proof_type: ProofType::RethZisk,
            witness_ms: Some(1_500),
            queue_wait_ms: Some(45_000),
            prove_ms: Some(8_000),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ProofComplete = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_backend_kind_serialization() {
        // Verify each BackendKind serializes to the expected lowercase string
        assert_eq!(
            serde_json::to_string(&BackendKind::Ere).unwrap(),
            r#""ere""#
        );
        assert_eq!(
            serde_json::to_string(&BackendKind::Mock).unwrap(),
            r#""mock""#
        );
        assert_eq!(
            serde_json::to_string(&BackendKind::Verifier).unwrap(),
            r#""verifier""#
        );
        assert_eq!(
            serde_json::to_string(&BackendKind::Cluster).unwrap(),
            r#""cluster""#
        );
    }

    #[test]
    fn test_proof_types_response_roundtrip() {
        // Verify the response type serializes and deserializes correctly
        let response = ProofTypesResponse {
            proof_types: vec![
                ProofTypeInfo {
                    proof_type: ProofType::RethZisk,
                    kind: BackendKind::Ere,
                    can_prove: true,
                    can_verify: true,
                },
                ProofTypeInfo {
                    proof_type: ProofType::EthrexZisk,
                    kind: BackendKind::Verifier,
                    can_prove: false,
                    can_verify: true,
                },
            ],
        };

        let json = serde_json::to_string(&response).unwrap();
        let parsed: ProofTypesResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(response, parsed);
        assert_eq!(parsed.proof_types.len(), 2);
    }
}

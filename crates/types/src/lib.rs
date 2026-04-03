//! Shared request/response types for the zkboost Proof Node API.
//!
//! This crate contains the types used by both the zkboost server and client
//! for REST API communication and SSE event streaming.

#![warn(unused_crate_dependencies)]

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
};

use serde::{Deserialize, Serialize};

mod new_payload_request;
mod proof_type;

#[rustfmt::skip]
pub use {
    lighthouse_types::{Hash256, MainnetEthSpec, Withdrawal},
    ssz::{Decode, Encode},
    tree_hash::TreeHash,
    new_payload_request::*,
    proof_type::*,
};

/// Query params for `POST /v1/execution_proof_requests`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProofRequestQuery {
    /// Comma-separated list of proof types to request.
    #[serde(
        deserialize_with = "comma_separated::deserialize",
        serialize_with = "comma_separated::serialize"
    )]
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

/// Query params for `POST /v1/execution_proof_verifications`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProofVerificationQuery {
    /// The root identifying the payload request.
    pub new_payload_request_root: Hash256,
    /// The proof type to verify.
    pub proof_type: ProofType,
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

    /// Returns the canonical SSE event name for this variant.
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

/// Custom serde for comma-separated `Vec<ProofType>` in query strings.
mod comma_separated {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::ProofType;

    pub(crate) fn serialize<S>(proof_types: &[ProofType], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s: String = proof_types
            .iter()
            .map(|proof_type| proof_type.as_str())
            .collect::<Vec<_>>()
            .join(",");
        serializer.serialize_str(&s)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<ProofType>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() {
            return Ok(Vec::new());
        }
        value
            .split(',')
            .map(|part| {
                part.trim()
                    .parse::<ProofType>()
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::ProofRequestQuery;

    #[test]
    fn test_empty_proof_types_deserializes_to_empty_vec() {
        let query: ProofRequestQuery = serde_json::from_str(r#"{"proof_types": ""}"#).unwrap();
        assert!(query.proof_types.is_empty());
    }
}

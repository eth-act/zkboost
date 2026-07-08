//! Execution layer proof type.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use ere_catalog::zkVMKind;
use libssz::{DecodeError, SszDecode, SszEncode};
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;

/// Execution layer proof type.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumIter,
)]
#[serde(into = "String", try_from = "String")]
pub enum ProofType {
    /// Ethrex with OpenVM backend.
    EthrexOpenVM,
    /// Ethrex with SP1 backend.
    EthrexSP1,
    /// Ethrex with Zisk backend.
    EthrexZisk,
    /// Reth with OpenVM backend.
    RethOpenVM,
    /// Reth with SP1 backend.
    RethSP1,
    /// Reth with Zisk backend.
    RethZisk,
}

/// Execution layer kind to use for stateless validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElKind {
    /// Reth
    Reth,
    /// Ethrex
    Ethrex,
}

impl ProofType {
    /// Returns the execution layer kind for this proof type.
    pub fn el_kind(&self) -> ElKind {
        match self {
            Self::EthrexOpenVM | Self::EthrexSP1 | Self::EthrexZisk => ElKind::Ethrex,
            Self::RethOpenVM | Self::RethSP1 | Self::RethZisk => ElKind::Reth,
        }
    }

    /// Returns the zkVM kind for this proof type.
    pub fn zkvm_kind(&self) -> zkVMKind {
        match self {
            Self::EthrexSP1 | Self::RethSP1 => zkVMKind::SP1,
            Self::EthrexOpenVM | Self::RethOpenVM => zkVMKind::OpenVM,
            Self::EthrexZisk | Self::RethZisk => zkVMKind::Zisk,
        }
    }

    /// Returns the string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EthrexOpenVM => "ethrex-openvm",
            Self::EthrexSP1 => "ethrex-sp1",
            Self::EthrexZisk => "ethrex-zisk",
            Self::RethOpenVM => "reth-openvm",
            Self::RethSP1 => "reth-sp1",
            Self::RethZisk => "reth-zisk",
        }
    }
}

impl SszEncode for ProofType {
    fn is_fixed_size() -> bool {
        true
    }

    fn fixed_size() -> usize {
        1
    }

    fn encoded_len(&self) -> usize {
        1
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        buf.push(*self as u8);
    }
}

impl SszDecode for ProofType {
    fn is_fixed_size() -> bool {
        true
    }

    fn fixed_size() -> usize {
        1
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() != 1 {
            return Err(DecodeError::InvalidFixedLength {
                expected: 1,
                got: bytes.len(),
            });
        }
        match bytes[0] {
            0 => Ok(Self::EthrexOpenVM),
            1 => Ok(Self::EthrexSP1),
            2 => Ok(Self::EthrexZisk),
            3 => Ok(Self::RethOpenVM),
            4 => Ok(Self::RethSP1),
            5 => Ok(Self::RethZisk),
            other => Err(DecodeError::InvalidUnionSelector(other)),
        }
    }
}

impl From<ProofType> for String {
    fn from(value: ProofType) -> Self {
        value.as_str().to_string()
    }
}

impl FromStr for ProofType {
    type Err = ProofTypeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "ethrex-openvm" => Self::EthrexOpenVM,
            "ethrex-sp1" => Self::EthrexSP1,
            "ethrex-zisk" => Self::EthrexZisk,
            "reth-openvm" => Self::RethOpenVM,
            "reth-sp1" => Self::RethSP1,
            "reth-zisk" => Self::RethZisk,
            _ => return Err(ProofTypeParseError(s.to_string())),
        })
    }
}

impl TryFrom<String> for ProofType {
    type Error = ProofTypeParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl Display for ProofType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Parse error for invalid proof type values.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ProofTypeParseError(String);

impl Display for ProofTypeParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let unsupported = &self.0;
        let supported =
            Vec::from_iter(ProofType::iter().map(|proof_type| proof_type.as_str())).join(", ");
        write!(
            f,
            "Unsupported proof type `{unsupported}`, expect one of [{supported}]",
        )
    }
}

impl Error for ProofTypeParseError {}

#[cfg(test)]
mod tests {
    use libssz::{DecodeError, SszDecode, SszEncode};
    use strum::IntoEnumIterator;

    use crate::ProofType;

    #[test]
    fn test_proof_type_ssz_roundtrip() {
        for proof_type in ProofType::iter() {
            let encoded = proof_type.to_ssz();
            assert_eq!(encoded.len(), 1);
            assert_eq!(ProofType::from_ssz_bytes(&encoded).unwrap(), proof_type);
        }
    }

    #[test]
    fn test_proof_type_ssz_rejects_unknown_discriminant() {
        assert_eq!(
            ProofType::from_ssz_bytes(&[6]),
            Err(DecodeError::InvalidUnionSelector(6))
        );
        assert!(matches!(
            ProofType::from_ssz_bytes(&[0, 0]),
            Err(DecodeError::InvalidFixedLength { .. })
        ));
    }
}

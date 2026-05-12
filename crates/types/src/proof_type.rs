//! Execution layer proof type.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use ere_catalog::zkVMKind;
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
    /// Ethrex with RISC Zero backend.
    EthrexRisc0,
    /// Ethrex with SP1 backend.
    EthrexSP1,
    /// Ethrex with Zisk backend.
    EthrexZisk,
    /// Reth with OpenVM backend.
    RethOpenVM,
    /// Reth with RISC Zero backend.
    RethRisc0,
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
            Self::EthrexRisc0 | Self::EthrexSP1 | Self::EthrexZisk => ElKind::Ethrex,
            Self::RethOpenVM | Self::RethRisc0 | Self::RethSP1 | Self::RethZisk => ElKind::Reth,
        }
    }

    /// Returns the zkVM kind for this proof type.
    pub fn zkvm_kind(&self) -> zkVMKind {
        match self {
            Self::EthrexRisc0 | Self::RethRisc0 => zkVMKind::Risc0,
            Self::EthrexSP1 | Self::RethSP1 => zkVMKind::SP1,
            Self::RethOpenVM => zkVMKind::OpenVM,
            Self::EthrexZisk | Self::RethZisk => zkVMKind::Zisk,
        }
    }

    /// Returns the canonical string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EthrexRisc0 => "ethrex-risc0",
            Self::EthrexSP1 => "ethrex-sp1",
            Self::EthrexZisk => "ethrex-zisk",
            Self::RethOpenVM => "reth-openvm",
            Self::RethRisc0 => "reth-risc0",
            Self::RethSP1 => "reth-sp1",
            Self::RethZisk => "reth-zisk",
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
            "ethrex-risc0" => Self::EthrexRisc0,
            "ethrex-sp1" => Self::EthrexSP1,
            "ethrex-zisk" => Self::EthrexZisk,
            "reth-openvm" => Self::RethOpenVM,
            "reth-risc0" => Self::RethRisc0,
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

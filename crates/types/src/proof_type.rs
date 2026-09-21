//! Execution layer proof type.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use ere_catalog::zkVMKind;
use serde::{Deserialize, Serialize};
use stateless_validator_catalog::StatelessValidatorKind;
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
#[repr(u8)]
pub enum ProofType {
    /// Ethrex with OpenVM backend.
    EthrexOpenVM = 1,
    /// Ethrex with SP1 backend.
    EthrexSP1 = 2,
    /// Ethrex with Zisk backend.
    EthrexZisk = 3,
    /// Reth with OpenVM backend.
    RethOpenVM = 4,
    /// Reth with SP1 backend.
    RethSP1 = 5,
    /// Reth with Zisk backend.
    RethZisk = 6,
    /// Zesu with Zisk backend.
    ZesuZisk = 7,
}

impl ProofType {
    /// Returns iterator of the enum variants.
    pub fn iter() -> impl Iterator<Item = Self> {
        <Self as IntoEnumIterator>::iter()
    }

    /// Returns the stateless validator kind for this proof type.
    pub fn stateless_validator_kind(&self) -> StatelessValidatorKind {
        match self {
            Self::EthrexSP1 | Self::EthrexOpenVM | Self::EthrexZisk => {
                StatelessValidatorKind::Ethrex
            }
            Self::RethSP1 | Self::RethOpenVM | Self::RethZisk => StatelessValidatorKind::Reth,
            Self::ZesuZisk => StatelessValidatorKind::Zesu,
        }
    }

    /// Returns the zkVM kind for this proof type.
    pub fn zkvm_kind(&self) -> zkVMKind {
        match self {
            Self::EthrexSP1 | Self::RethSP1 => zkVMKind::SP1,
            Self::EthrexOpenVM | Self::RethOpenVM => zkVMKind::OpenVM,
            Self::EthrexZisk | Self::RethZisk | Self::ZesuZisk => zkVMKind::Zisk,
        }
    }

    /// Returns the EIP-8025 execution proof type of the beacon chain.
    pub fn execution_proof_type(&self) -> u8 {
        *self as u8
    }

    /// Returns the string identifier of the proof type.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EthrexOpenVM => "ethrex-openvm",
            Self::EthrexSP1 => "ethrex-sp1",
            Self::EthrexZisk => "ethrex-zisk",
            Self::RethOpenVM => "reth-openvm",
            Self::RethSP1 => "reth-sp1",
            Self::RethZisk => "reth-zisk",
            Self::ZesuZisk => "zesu-zisk",
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
            "zesu-zisk" => Self::ZesuZisk,
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
            "unsupported proof type `{unsupported}`, expect one of [{supported}]",
        )
    }
}

impl Error for ProofTypeParseError {}

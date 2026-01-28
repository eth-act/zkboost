//! Ethereum Execution Layer proof type.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use ere_common::zkVMKind;
use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};

use crate::ElKind;

/// Execution Layer proof type.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, EnumIter,
)]
#[serde(into = "String", try_from = "String")]
#[repr(u8)]
pub enum ElProofType {
    /// EthrexRisc0
    EthrexRisc0,
    /// EthrexSP1
    EthrexSP1,
    /// EthrexZisk
    EthrexZisk,
    /// RethOpenVM
    RethOpenVM,
    /// RethPico
    RethPico,
    /// RethRisc0
    RethRisc0,
    /// RethSP1
    RethSP1,
    /// RethZisk
    RethZisk,
}

impl ElProofType {
    /// Returns `ElKind`.
    pub fn el(&self) -> ElKind {
        match self {
            Self::EthrexRisc0 | Self::EthrexSP1 | Self::EthrexZisk => ElKind::Ethrex,
            Self::RethOpenVM
            | Self::RethPico
            | Self::RethRisc0
            | Self::RethSP1
            | Self::RethZisk => ElKind::Reth,
        }
    }

    /// Returns `zkVMKind`.
    pub fn zkvm(&self) -> zkVMKind {
        match self {
            Self::EthrexRisc0 | Self::RethRisc0 => zkVMKind::Risc0,
            Self::EthrexSP1 | Self::RethSP1 => zkVMKind::SP1,
            Self::EthrexZisk | Self::RethZisk => zkVMKind::Zisk,
            Self::RethOpenVM => zkVMKind::OpenVM,
            Self::RethPico => zkVMKind::Pico,
        }
    }

    /// Returns proof ID.
    pub fn proof_id(&self) -> u8 {
        *self as u8
    }

    /// Returns string representation of the execution layer proof type.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EthrexRisc0 => "ethrex-risc0",
            Self::EthrexSP1 => "ethrex-sp1",
            Self::EthrexZisk => "ethrex-zisk",
            Self::RethOpenVM => "reth-openvm",
            Self::RethPico => "reth-pico",
            Self::RethRisc0 => "reth-risc0",
            Self::RethSP1 => "reth-sp1",
            Self::RethZisk => "reth-zisk",
        }
    }
}

impl From<ElProofType> for String {
    fn from(value: ElProofType) -> Self {
        value.as_str().to_string()
    }
}

impl FromStr for ElProofType {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "ethrex-risc0" => Self::EthrexRisc0,
            "ethrex-sp1" => Self::EthrexSP1,
            "ethrex-zisk" => Self::EthrexZisk,
            "reth-openvm" => Self::RethOpenVM,
            "reth-pico" => Self::RethPico,
            "reth-risc0" => Self::RethRisc0,
            "reth-sp1" => Self::RethSP1,
            "reth-zisk" => Self::RethZisk,
            _ => return Err(ParseError(s.to_string())),
        })
    }
}

impl TryFrom<String> for ElProofType {
    type Error = ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl Display for ElProofType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Parse error for invalid execution layer proof type strings.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ParseError(String);

impl From<&str> for ParseError {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl Display for ParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let unsupported = &self.0;
        let supported = Vec::from_iter(ElProofType::iter().map(|k| k.as_str())).join(", ");
        write!(
            f,
            "Unsupported compiler kind `{unsupported}`, expect one of [{supported}]",
        )
    }
}

impl Error for ParseError {}

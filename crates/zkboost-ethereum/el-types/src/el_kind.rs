//! Ethereum Execution Layer kinds.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
};

use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};

/// Execution layer kind to use to do stateless validation.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    EnumIter,
    EnumString,
    IntoStaticStr,
    Display,
)]
#[serde(into = "String", try_from = "String")]
#[strum(
    ascii_case_insensitive,
    serialize_all = "lowercase",
    parse_err_fn = ParseError::from,
    parse_err_ty = ParseError
)]
pub enum ElKind {
    /// Reth
    Reth,
    /// Ethrex
    Ethrex,
}

impl ElKind {
    /// Returns string representation of the execution layer kind.
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

impl From<ElKind> for String {
    fn from(value: ElKind) -> Self {
        value.as_str().to_string()
    }
}

impl TryFrom<String> for ElKind {
    type Error = ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// Parse error for invalid execution layer kind strings.
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
        let supported = Vec::from_iter(ElKind::iter().map(|k| k.as_str())).join(", ");
        write!(
            f,
            "Unsupported compiler kind `{unsupported}`, expect one of [{supported}]",
        )
    }
}

impl Error for ParseError {}

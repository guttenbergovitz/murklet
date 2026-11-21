//! Explicit error types for loading and validating declarative Murklet configuration.

use std::fmt;

/// Failures produced when reading, parsing, or validating a Murklet policy/config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    /// Reading the configuration file from disk failed.
    ReadFile(String),
    /// Parsing TOML syntax or schema failed (including unknown fields).
    ParseToml(String),
    /// Parsed configuration violated one or more semantic or security invariants.
    Validation(String),
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadFile(reason) => write!(f, "failed to read configuration file: {reason}"),
            Self::ParseToml(reason) => write!(f, "invalid TOML configuration: {reason}"),
            Self::Validation(reason) => write!(f, "invalid configuration policy: {reason}"),
        }
    }
}

impl std::error::Error for PolicyError {}

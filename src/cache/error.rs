//! Explicit error types for variant caching backends.

use std::fmt;

/// Failures produced when initialising or accessing a variant cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    /// Cache root directory configuration is invalid (not a directory or cannot be prepared).
    InvalidRoot(String),
    /// Variant payload exceeded the configured per-entry byte ceiling.
    EntryTooLarge {
        /// Actual or observed payload size in bytes.
        size: usize,
        /// Configured maximum permitted size per cache entry in bytes.
        max: usize,
    },
    /// Caller's memory-budget reservation callback rejected allocating the entry.
    BudgetExhausted,
    /// Unexpected filesystem or storage I/O failure.
    Io(String),
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot(reason) => write!(f, "invalid cache root: {reason}"),
            Self::EntryTooLarge { size, max } => {
                write!(
                    f,
                    "cache entry size {size} bytes exceeds maximum of {max} bytes"
                )
            }
            Self::BudgetExhausted => {
                f.write_str("in-flight memory budget exhausted before reading cached entry")
            }
            Self::Io(reason) => write!(f, "cache I/O failure: {reason}"),
        }
    }
}

impl std::error::Error for CacheError {}

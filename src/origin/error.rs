//! Explicit error types for origin adapters and source fetching.

use std::fmt;

/// Failures produced when configuring or fetching from an image origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginError {
    /// Origin root configuration is invalid (missing, unreadable, or not a directory).
    InvalidRoot(String),
    /// Caller supplied a malformed or unsafe asset path or URL.
    InvalidPath(String),
    /// Resolved target path escaped the configured origin root (for example via a symlink).
    PathEscapesRoot,
    /// Remote host is not permitted by the configured origin allowlist.
    HostNotAllowed,
    /// Remote URL or DNS resolution pointed to a forbidden IP range (SSRF defence).
    ForbiddenIpAddress,
    /// Remote origin exceeded the maximum permitted number of HTTP redirects.
    TooManyRedirects {
        /// Configured maximum redirect hops.
        max: u8,
    },
    /// Remote origin responded with a non-success HTTP status code.
    UpstreamStatus(u16),
    /// Upstream request timed out before completing.
    Timeout,
    /// Requested asset does not exist at the origin.
    NotFound,
    /// Resolved target is not a regular file (for example a directory or special node).
    NotRegularFile,
    /// Source asset exceeded the configured byte ceiling.
    PayloadTooLarge {
        /// Observed size in bytes (at least `max + 1` when aborted mid-stream).
        size: usize,
        /// Configured maximum permitted size in bytes.
        max: usize,
    },
    /// Unexpected I/O or transport failure while reading from the origin.
    Io(String),
}

impl fmt::Display for OriginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot(reason) => write!(f, "invalid origin root: {reason}"),
            Self::InvalidPath(reason) => write!(f, "invalid asset path: {reason}"),
            Self::PathEscapesRoot => f.write_str("requested path escapes configured origin root"),
            Self::HostNotAllowed => f.write_str("remote host is not permitted by origin allowlist"),
            Self::ForbiddenIpAddress => {
                f.write_str("remote host resolved to a forbidden IP address")
            }
            Self::TooManyRedirects { max } => {
                write!(f, "remote origin exceeded maximum of {max} redirects")
            }
            Self::UpstreamStatus(code) => write!(f, "upstream origin returned HTTP status {code}"),
            Self::Timeout => f.write_str("upstream origin request timed out"),
            Self::NotFound => f.write_str("asset not found at origin"),
            Self::NotRegularFile => f.write_str("requested origin target is not a regular file"),
            Self::PayloadTooLarge { size, max } => {
                write!(
                    f,
                    "origin payload size {size} bytes exceeds maximum of {max} bytes"
                )
            }
            Self::Io(reason) => write!(f, "origin I/O failure: {reason}"),
        }
    }
}

impl std::error::Error for OriginError {}

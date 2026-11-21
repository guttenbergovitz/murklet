//! Explicit error types for domain validation and invariant violations.

use std::fmt;

/// Identifies whether a dimension validation failure occurred on width or height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    /// Horizontal image axis (`w`).
    Width,
    /// Vertical image axis (`h`).
    Height,
}

impl Axis {
    /// Returns the lowercase axis label used in diagnostics and error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Width => "width",
            Self::Height => "height",
        }
    }
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Errors produced when constructing or validating domain value types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// A requested width or height is outside the permitted `1..=max` range.
    DimensionOutOfRange {
        /// Which axis failed validation.
        axis: Axis,
        /// Rejected value supplied by the caller.
        value: u32,
        /// Inclusive upper bound enforced by configuration or baseline limits.
        max: u32,
    },
    /// A source image reported a zero width or height.
    ZeroSourceDimension {
        /// Which axis was zero.
        axis: Axis,
    },
    /// Requested lossy encoding quality is outside `1..=100`.
    QualityOutOfRange {
        /// Rejected quality value.
        value: u8,
    },
    /// Requested output image format is not supported.
    UnsupportedFormat(String),
    /// Requested resampling filter is not supported.
    UnsupportedFilter(String),
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DimensionOutOfRange { axis, value, max } => {
                write!(f, "{axis} must be between 1 and {max}, got {value}")
            }
            Self::ZeroSourceDimension { axis } => {
                write!(f, "source image {axis} must be greater than zero")
            }
            Self::QualityOutOfRange { value } => {
                write!(f, "quality must be between 1 and 100, got {value}")
            }
            Self::UnsupportedFormat(format) => {
                write!(f, "unsupported output format: '{format}'")
            }
            Self::UnsupportedFilter(filter) => {
                write!(f, "unsupported resize filter: '{filter}'")
            }
        }
    }
}

impl std::error::Error for DomainError {}

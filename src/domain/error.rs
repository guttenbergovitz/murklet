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
    /// Requested fit mode is not supported.
    UnsupportedFitMode(String),
    /// Requested crop gravity or focal point is not supported.
    UnsupportedGravity(String),
    /// Requested rotation angle is not one of `0`, `90`, `180`, `270`.
    UnsupportedRotation(u16),
    /// Requested device pixel ratio is outside `1.0..=3.0` or malformed.
    InvalidDpr(String),
    /// Requested background colour is not valid 3- or 6-character RGB hex.
    InvalidBackground(String),
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
            Self::UnsupportedFitMode(mode) => {
                write!(f, "unsupported fit mode: '{mode}'")
            }
            Self::UnsupportedGravity(gravity) => {
                write!(f, "unsupported gravity: '{gravity}'")
            }
            Self::UnsupportedRotation(degrees) => {
                write!(f, "rotation must be 0, 90, 180, or 270, got {degrees}")
            }
            Self::InvalidDpr(raw) => {
                write!(f, "dpr must be between 1.0 and 3.0, got '{raw}'")
            }
            Self::InvalidBackground(raw) => {
                write!(f, "background must be 3- or 6-digit RGB hex, got '{raw}'")
            }
        }
    }
}

impl std::error::Error for DomainError {}

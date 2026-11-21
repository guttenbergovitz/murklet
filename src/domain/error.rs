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
    /// Requested mirror flip mode is not supported.
    UnsupportedFlip(String),
    /// Requested blur sigma is outside `0.5..=20.0` or malformed.
    InvalidBlur(String),
    /// Requested sharpen sigma is outside `0.5..=10.0` or malformed.
    InvalidSharpen(String),
    /// Requested brightness offset is outside `-100..=100`.
    InvalidBrightness(i16),
    /// Requested contrast adjustment is outside `-100..=100`.
    InvalidContrast(i16),
    /// Requested saturation adjustment is outside `-100..=100`.
    InvalidSaturation(i16),
    /// Requested hue rotation is outside `-360..=360`.
    InvalidHue(i32),
    /// Requested gamma value is outside `0.2..=5.0` or malformed.
    InvalidGamma(String),
    /// Requested sepia intensity is outside `0..=100` or malformed.
    InvalidSepia(String),
    /// Requested monochrome tint colour is not valid 3- or 6-character RGB hex.
    InvalidTint(String),
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
            Self::UnsupportedFlip(raw) => {
                write!(f, "unsupported flip mode: '{raw}' (use h, v, or hv)")
            }
            Self::InvalidBlur(raw) => {
                write!(f, "blur sigma must be between 0.5 and 20.0, got '{raw}'")
            }
            Self::InvalidSharpen(raw) => {
                write!(f, "sharpen sigma must be between 0.5 and 10.0, got '{raw}'")
            }
            Self::InvalidBrightness(val) => {
                write!(f, "brightness must be between -100 and 100, got {val}")
            }
            Self::InvalidContrast(val) => {
                write!(f, "contrast must be between -100 and 100, got {val}")
            }
            Self::InvalidSaturation(val) => {
                write!(f, "saturation must be between -100 and 100, got {val}")
            }
            Self::InvalidHue(val) => {
                write!(f, "hue rotation must be between -360 and 360, got {val}")
            }
            Self::InvalidGamma(raw) => {
                write!(f, "gamma must be between 0.2 and 5.0, got '{raw}'")
            }
            Self::InvalidSepia(raw) => {
                write!(
                    f,
                    "sepia must be between 0 and 100 (or true/false), got '{raw}'"
                )
            }
            Self::InvalidTint(raw) => {
                write!(f, "tint must be 3- or 6-digit RGB hex, got '{raw}'")
            }
        }
    }
}

impl std::error::Error for DomainError {}

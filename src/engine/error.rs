//! Explicit error types for image decoding, limit enforcement, and encoding.

use std::fmt;

use crate::domain::{ImageDimensions, OutputFormat};

/// Failures produced by the image-processing engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// Raw encoded input exceeded the configured byte ceiling.
    InputTooLarge {
        /// Actual input size in bytes.
        size: usize,
        /// Configured maximum input size in bytes.
        max: usize,
    },
    /// Input bytes did not match any supported image format signature.
    UnsupportedInputFormat,
    /// Requested output format is not supported by the current engine build.
    UnsupportedOutputFormat(OutputFormat),
    /// Source image header dimensions exceeded the configured maximum edge length.
    SourceDimensionLimitExceeded {
        /// Dimensions reported by the image header.
        dimensions: ImageDimensions,
        /// Configured maximum permitted width or height.
        max_dimension: u32,
    },
    /// Source image header pixel count exceeded the configured decompression-bomb guard.
    DecodedPixelLimitExceeded {
        /// Total decoded pixel count (`width * height`).
        pixels: u64,
        /// Configured maximum permitted pixel count.
        max_pixels: u64,
    },
    /// Decoding the source image failed (corrupt payload or decoder error).
    DecodeFailed(String),
    /// Encoding the transformed image into the requested output format failed.
    EncodeFailed(String),
    /// Requested logical watermark name is not registered in the active policy.
    UnknownWatermark(String),
    /// Loading or decoding a configured watermark image at startup failed.
    WatermarkLoadFailed {
        /// Logical watermark name from `[watermarks]`.
        name: String,
        /// Human-readable cause of the failure.
        reason: String,
    },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { size, max } => {
                write!(f, "input size {size} bytes exceeds maximum of {max} bytes")
            }
            Self::UnsupportedInputFormat => f.write_str("unsupported or unrecognised image format"),
            Self::UnsupportedOutputFormat(format) => {
                write!(
                    f,
                    "output format '{format}' is not supported by this engine"
                )
            }
            Self::SourceDimensionLimitExceeded {
                dimensions,
                max_dimension,
            } => write!(
                f,
                "source dimensions {}x{} exceed maximum edge length of {max_dimension}",
                dimensions.width(),
                dimensions.height()
            ),
            Self::DecodedPixelLimitExceeded { pixels, max_pixels } => write!(
                f,
                "source image has {pixels} pixels, exceeding limit of {max_pixels}"
            ),
            Self::DecodeFailed(reason) => write!(f, "failed to decode image: {reason}"),
            Self::EncodeFailed(reason) => write!(f, "failed to encode image: {reason}"),
            Self::UnknownWatermark(name) => write!(f, "unknown watermark '{name}'"),
            Self::WatermarkLoadFailed { name, reason } => {
                write!(f, "failed to load watermark '{name}': {reason}")
            }
        }
    }
}

impl std::error::Error for EngineError {}

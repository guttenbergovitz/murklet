//! Resource limits and animation fallback policy enforced by the image engine.

/// Default maximum encoded input size (50 MiB).
pub const DEFAULT_MAX_INPUT_BYTES: usize = 50 * 1024 * 1024;

/// Default maximum source width or height accepted before full decoding (16,384 px).
pub const DEFAULT_MAX_SOURCE_DIMENSION: u32 = 16_384;

/// Default maximum decoded pixel count (`width * height`) to defend against decompression bombs.
pub const DEFAULT_MAX_DECODED_PIXELS: u64 = 64_000_000;

/// Fallback behaviour when the source image contains multiple animation frames (ADR 0001).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AnimationPolicy {
    /// Decode and transform only the first frame (default in v0.1).
    #[default]
    FirstFrame,
    /// Return the original animated bytes unchanged when the input is within byte limits.
    Passthrough,
}

/// Resource bounds and fallback rules applied to every image transformation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EngineLimits {
    /// Maximum encoded input payload size in bytes.
    pub max_input_bytes: usize,
    /// Maximum source width or height in pixels.
    pub max_source_dimension: u32,
    /// Maximum total decoded pixels (`width * height`) allowed before full decode.
    pub max_decoded_pixels: u64,
    /// Fallback policy for animated GIF/WebP inputs.
    pub animation_policy: AnimationPolicy,
}

impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_source_dimension: DEFAULT_MAX_SOURCE_DIMENSION,
            max_decoded_pixels: DEFAULT_MAX_DECODED_PIXELS,
            animation_policy: AnimationPolicy::FirstFrame,
        }
    }
}

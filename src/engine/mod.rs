//! Image decoding, transformation, and encoding behind a replaceable internal
//! boundary.

pub mod error;
pub mod limits;
pub mod pipeline;

pub use error::EngineError;
pub use limits::{
    AnimationPolicy, DEFAULT_MAX_DECODED_PIXELS, DEFAULT_MAX_INPUT_BYTES,
    DEFAULT_MAX_SOURCE_DIMENSION, EngineLimits,
};
pub use pipeline::{TransformOutput, transform};

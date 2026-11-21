//! Core domain types, canonical transformation specifications, and domain errors.
//!
//! This module has no dependency on HTTP transport, storage backends, or image
//! processing libraries so invariants remain pure and fast to test.

pub mod error;
pub mod transform;

pub use error::{Axis, DomainError};
pub use transform::{
    CANONICAL_SPEC_VERSION, DEFAULT_MAX_DIMENSION, Dimension, ImageDimensions, OutputFormat,
    Quality, ResizeFilter, SelectedFilter, TargetDimensions, TransformSpec,
};

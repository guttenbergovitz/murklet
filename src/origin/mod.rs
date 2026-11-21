//! Image source adapters for fetching bytes from trusted origins (HTTP(S),
//! local filesystem, and S3-compatible object storage) under strict safety guards.

pub mod error;
pub mod local;

pub use error::OriginError;
pub use local::{FetchedImage, LocalFsOrigin};

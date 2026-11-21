//! Variant caching, canonical key storage, freshness semantics, and concurrent
//! request coalescing.

pub mod error;
pub mod fs;
pub mod key;

pub use error::CacheError;
pub use fs::{CachedVariant, FsCache};
pub use key::{CacheKey, compute_etag};

//! Variant caching, canonical key storage, freshness semantics, and concurrent
//! request coalescing.

pub mod coalesce;
pub mod error;
pub mod fs;
pub mod invalidation;
pub mod key;

pub use coalesce::{Coalesced, Coalescer};
pub use error::CacheError;
pub use fs::{CachedVariant, EvictionReport, FsCache};
pub use invalidation::{FlightToken, InvalidationTracker};
pub use key::{CacheKey, compute_etag, source_digest_hex};

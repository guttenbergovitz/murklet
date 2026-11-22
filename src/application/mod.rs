//! Application orchestration layer coordinating policy resolution, origin fetching,
//! resource isolation, image transformation, caching, and invalidation.
//!
//! Kept separate from [`crate::http`] so transport handlers deal exclusively with
//! HTTP parsing, headers, conditional responses, and status codes.

pub mod load;

pub use load::{
    AppState, LoadExecution, MissOutcome, ResolvedLoad, cleanup_all_caches, execute_load,
    purge_source_entries, resolve_and_validate_load,
};

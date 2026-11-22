//! Security and delivery policies governing allowed origins, transformations,
//! and resource limits.

pub mod config;
pub mod error;
pub mod origin_policy;
pub mod presets;
pub mod signing;

pub use config::{
    AnimationPolicyConfig, CacheConfig, DEFAULT_CACHE_CONTROL, DEFAULT_MAX_CONCURRENT_FETCHES,
    DEFAULT_MAX_CONCURRENT_TRANSFORMS, DEFAULT_MAX_IN_FLIGHT_SOURCE_BYTES,
    DEFAULT_MAX_PENDING_TRANSFORMS, DEFAULT_TRANSFORM_QUEUE_TIMEOUT_SECS, HttpOriginSection,
    LimitsConfig, LocalOriginConfig, LogFormatConfig, MurkletConfig, OriginsConfig,
    S3OriginSection, ServerConfig, SourceCacheConfig,
};
pub use error::PolicyError;
pub use origin_policy::{OriginPolicies, OriginPolicy, OriginPolicyConfig};
pub use presets::{PresetConfig, PresetRegistry, ValidatedPreset};
pub use signing::{RequestSigner, canonicalise_query_for_signing};

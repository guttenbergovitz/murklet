//! Security and delivery policies governing allowed origins, transformations,
//! and resource limits.

pub mod config;
pub mod error;
pub mod presets;
pub mod signing;

pub use config::{
    AnimationPolicyConfig, CacheConfig, DEFAULT_CACHE_CONTROL, DEFAULT_MAX_CONCURRENT_TRANSFORMS,
    HttpOriginSection, LimitsConfig, LocalOriginConfig, LogFormatConfig, MurkletConfig,
    OriginsConfig, S3OriginSection, ServerConfig,
};
pub use error::PolicyError;
pub use presets::{PresetConfig, PresetRegistry, ValidatedPreset};
pub use signing::{RequestSigner, canonicalise_query_for_signing};

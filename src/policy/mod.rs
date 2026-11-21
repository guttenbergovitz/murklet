//! Security and delivery policies governing allowed origins, transformations,
//! and resource limits.

pub mod config;
pub mod error;

pub use config::{
    AnimationPolicyConfig, CacheConfig, HttpOriginSection, LimitsConfig, LocalOriginConfig,
    MurkletConfig, OriginsConfig, ServerConfig,
};
pub use error::PolicyError;

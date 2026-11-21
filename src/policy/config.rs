//! Declarative TOML configuration and startup policy validation.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::domain::DEFAULT_MAX_DIMENSION;
use crate::engine::{
    AnimationPolicy, DEFAULT_MAX_DECODED_PIXELS, DEFAULT_MAX_INPUT_BYTES,
    DEFAULT_MAX_SOURCE_DIMENSION, EngineLimits,
};
use crate::origin::{DEFAULT_MAX_REDIRECTS, HostAllowlist, HttpOriginConfig};
use crate::policy::error::PolicyError;
use crate::policy::signing::RequestSigner;

/// Default `Cache-Control` header applied to served image variants.
pub const DEFAULT_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// Default upper bound on concurrent CPU-bound image transformations.
pub const DEFAULT_MAX_CONCURRENT_TRANSFORMS: usize = 8;

/// Top-level validated Murklet service and policy configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MurkletConfig {
    /// HTTP server binding, caching headers, and administrative settings.
    #[serde(default)]
    pub server: ServerConfig,
    /// Filesystem variant cache settings.
    #[serde(default)]
    pub cache: CacheConfig,
    /// Resource limits and animation fallback policy.
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Enabled origins and their access policies.
    #[serde(default)]
    pub origins: OriginsConfig,
}

/// Structured log output format (`server.log_format`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormatConfig {
    /// Human-readable structured logs for local development.
    #[default]
    Pretty,
    /// Machine-readable JSON log lines for production log aggregators.
    Json,
}

/// HTTP transport and operational settings (`[server]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Socket address to bind (default `"127.0.0.1:9876"`).
    #[serde(default = "default_bind_addr")]
    pub bind: SocketAddr,
    /// `Cache-Control` header value emitted on image responses.
    #[serde(default = "default_cache_control")]
    pub cache_control: String,
    /// Negotiate output format from the HTTP `Accept` header when `f` is omitted (default `false`).
    #[serde(default)]
    pub accept_negotiation: bool,
    /// Expose Prometheus metrics at `GET /metrics` (default `true`).
    #[serde(default = "default_true")]
    pub metrics_enabled: bool,
    /// Structured log format (`"pretty"` or `"json"`, default `"pretty"`).
    #[serde(default)]
    pub log_format: LogFormatConfig,
    /// Emit extended `X-Original-*` / `X-Processed-*` headers on cache MISS (default `false`).
    #[serde(default)]
    pub diagnostic_headers: bool,
    /// Optional secret token required by `POST /cache/cleanup`.
    #[serde(default)]
    pub cleanup_token: Option<String>,
    /// Optional HMAC-SHA256 secret requiring a valid `sig` query parameter on `GET /load`.
    #[serde(default)]
    pub signing_secret: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind_addr(),
            cache_control: default_cache_control(),
            accept_negotiation: false,
            metrics_enabled: true,
            log_format: LogFormatConfig::Pretty,
            diagnostic_headers: false,
            cleanup_token: None,
            signing_secret: None,
        }
    }
}

/// Filesystem variant cache settings (`[cache]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Directory where sharded cache entries are stored.
    #[serde(default = "default_cache_dir")]
    pub dir: PathBuf,
    /// Maximum size of a single cached variant in bytes.
    #[serde(default = "default_max_input_bytes")]
    pub max_entry_bytes: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dir: default_cache_dir(),
            max_entry_bytes: default_max_input_bytes(),
        }
    }
}

/// Processing resource limits and animation policy (`[limits]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// Maximum source payload size in bytes.
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: usize,
    /// Maximum source image width or height accepted before full decoding.
    #[serde(default = "default_max_source_dimension")]
    pub max_source_dimension: u32,
    /// Maximum target width or height accepted in query parameters.
    #[serde(default = "default_max_target_dimension")]
    pub max_target_dimension: u32,
    /// Maximum decoded pixel count (`width * height`) allowed before full decode.
    #[serde(default = "default_max_decoded_pixels")]
    pub max_decoded_pixels: u64,
    /// Maximum number of concurrent CPU-bound image transformations (backpressure guard).
    #[serde(default = "default_max_concurrent_transforms")]
    pub max_concurrent_transforms: usize,
    /// Fallback policy for animated GIF/WebP inputs.
    #[serde(default)]
    pub animation_policy: AnimationPolicyConfig,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_input_bytes: default_max_input_bytes(),
            max_source_dimension: default_max_source_dimension(),
            max_target_dimension: default_max_target_dimension(),
            max_decoded_pixels: default_max_decoded_pixels(),
            max_concurrent_transforms: default_max_concurrent_transforms(),
            animation_policy: AnimationPolicyConfig::FirstFrame,
        }
    }
}

/// Declarative TOML representation of [`AnimationPolicy`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimationPolicyConfig {
    /// Decode and transform only the first frame.
    #[default]
    FirstFrame,
    /// Pass animated GIF/WebP bytes through unchanged within byte limits.
    Passthrough,
}

impl From<AnimationPolicyConfig> for AnimationPolicy {
    fn from(value: AnimationPolicyConfig) -> Self {
        match value {
            AnimationPolicyConfig::FirstFrame => Self::FirstFrame,
            AnimationPolicyConfig::Passthrough => Self::Passthrough,
        }
    }
}

/// Origin adapter configuration (`[origins]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginsConfig {
    /// Local filesystem origin configuration (`[origins.local]`).
    #[serde(default)]
    pub local: LocalOriginConfig,
    /// Remote HTTP(S) origin configuration (`[origins.http]`).
    #[serde(default)]
    pub http: HttpOriginSection,
}

/// Local filesystem origin settings (`[origins.local]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalOriginConfig {
    /// Whether local filesystem asset serving is enabled (default `true`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Root directory containing source assets.
    #[serde(default = "default_local_root")]
    pub root: PathBuf,
}

impl Default for LocalOriginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root: default_local_root(),
        }
    }
}

/// Remote HTTP(S) origin settings (`[origins.http]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpOriginSection {
    /// Whether remote HTTP(S) fetching is enabled (default `false`).
    #[serde(default)]
    pub enabled: bool,
    /// Explicit allowlist of permitted remote hosts (for example `["cdn.example.com", "*.assets.example.org"]`).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Total upstream HTTP fetch timeout in seconds (default `15`).
    #[serde(default = "default_http_timeout_secs")]
    pub timeout_secs: u64,
    /// Upstream TCP/TLS connect timeout in seconds (default `5`).
    #[serde(default = "default_http_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Maximum number of HTTP redirects followed per fetch (default `5`).
    #[serde(default = "default_max_redirects")]
    pub max_redirects: u8,
}

impl Default for HttpOriginSection {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_hosts: Vec::new(),
            timeout_secs: default_http_timeout_secs(),
            connect_timeout_secs: default_http_connect_timeout_secs(),
            max_redirects: default_max_redirects(),
        }
    }
}

impl MurkletConfig {
    /// Parses and validates a TOML configuration string.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::ParseToml`] on syntax or unknown-field errors, or
    /// [`PolicyError::Validation`] if any semantic or security invariant is violated.
    pub fn from_toml_str(raw: &str) -> Result<Self, PolicyError> {
        let mut config: Self =
            toml::from_str(raw).map_err(|err| PolicyError::ParseToml(err.to_string()))?;

        // Normalise blank secrets to None so optional auth features stay fail-closed/disabled.
        if config
            .server
            .cleanup_token
            .as_deref()
            .is_some_and(str::is_empty)
        {
            config.server.cleanup_token = None;
        }
        if config
            .server
            .signing_secret
            .as_deref()
            .is_some_and(str::is_empty)
        {
            config.server.signing_secret = None;
        }

        config.validate()?;
        Ok(config)
    }

    /// Reads, parses, and validates a TOML configuration file from disk.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::ReadFile`], [`PolicyError::ParseToml`], or [`PolicyError::Validation`].
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let raw = fs::read_to_string(path.as_ref())
            .map_err(|err| PolicyError::ReadFile(err.to_string()))?;
        Self::from_toml_str(&raw)
    }

    /// Converts `[limits]` into [`EngineLimits`] consumed by the image engine.
    #[must_use]
    pub fn engine_limits(&self) -> EngineLimits {
        EngineLimits {
            max_input_bytes: self.limits.max_input_bytes,
            max_source_dimension: self.limits.max_source_dimension,
            max_decoded_pixels: self.limits.max_decoded_pixels,
            animation_policy: self.limits.animation_policy.into(),
        }
    }

    /// Builds an optional [`RequestSigner`] when `server.signing_secret` is configured.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::Validation`] if the configured secret is empty.
    pub fn request_signer(&self) -> Result<Option<RequestSigner>, PolicyError> {
        self.server
            .signing_secret
            .as_deref()
            .map(|secret| RequestSigner::new(secret.as_bytes().to_vec()))
            .transpose()
    }

    /// Builds an [`HttpOriginConfig`] when `[origins.http]` is enabled, or `None` when disabled.
    #[must_use]
    pub fn http_origin_config(&self) -> Option<HttpOriginConfig> {
        if !self.origins.http.enabled {
            return None;
        }

        let allowlist = HostAllowlist::new(&self.origins.http.allowed_hosts);
        let mut cfg = HttpOriginConfig::new(allowlist, self.limits.max_input_bytes);
        cfg.timeout = Duration::from_secs(self.origins.http.timeout_secs);
        cfg.connect_timeout = Duration::from_secs(self.origins.http.connect_timeout_secs);
        cfg.max_redirects = self.origins.http.max_redirects;
        Some(cfg)
    }

    fn validate(&self) -> Result<(), PolicyError> {
        if self.server.cache_control.trim().is_empty()
            || !self.server.cache_control.is_ascii()
            || self.server.cache_control.contains(['\r', '\n'])
        {
            return Err(PolicyError::Validation(
                "server.cache_control must be a non-empty single-line ASCII value".to_owned(),
            ));
        }

        if self.cache.dir.as_os_str().is_empty() {
            return Err(PolicyError::Validation(
                "cache.dir must not be empty".to_owned(),
            ));
        }
        if self.cache.max_entry_bytes == 0 {
            return Err(PolicyError::Validation(
                "cache.max_entry_bytes must be greater than zero".to_owned(),
            ));
        }

        if self.limits.max_input_bytes == 0 {
            return Err(PolicyError::Validation(
                "limits.max_input_bytes must be greater than zero".to_owned(),
            ));
        }
        if self.limits.max_source_dimension == 0 {
            return Err(PolicyError::Validation(
                "limits.max_source_dimension must be greater than zero".to_owned(),
            ));
        }
        if self.limits.max_target_dimension == 0 {
            return Err(PolicyError::Validation(
                "limits.max_target_dimension must be greater than zero".to_owned(),
            ));
        }
        if self.limits.max_decoded_pixels == 0 {
            return Err(PolicyError::Validation(
                "limits.max_decoded_pixels must be greater than zero".to_owned(),
            ));
        }
        if self.limits.max_concurrent_transforms == 0 {
            return Err(PolicyError::Validation(
                "limits.max_concurrent_transforms must be greater than zero".to_owned(),
            ));
        }

        if !self.origins.local.enabled && !self.origins.http.enabled {
            return Err(PolicyError::Validation(
                "at least one origin (origins.local or origins.http) must be enabled".to_owned(),
            ));
        }

        if self.origins.local.enabled && self.origins.local.root.as_os_str().is_empty() {
            return Err(PolicyError::Validation(
                "origins.local.root must not be empty when origins.local is enabled".to_owned(),
            ));
        }

        if self.origins.http.enabled {
            let allowlist = HostAllowlist::new(&self.origins.http.allowed_hosts);
            if allowlist.is_empty() {
                return Err(PolicyError::Validation(
                    "origins.http.allowed_hosts must contain at least one valid host when origins.http is enabled".to_owned(),
                ));
            }
            if self.origins.http.timeout_secs == 0 || self.origins.http.connect_timeout_secs == 0 {
                return Err(PolicyError::Validation(
                    "origins.http timeouts must be greater than zero".to_owned(),
                ));
            }
            if self.origins.http.connect_timeout_secs > self.origins.http.timeout_secs {
                return Err(PolicyError::Validation(
                    "origins.http.connect_timeout_secs must not exceed timeout_secs".to_owned(),
                ));
            }
        }

        Ok(())
    }
}

fn default_bind_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9876))
}

fn default_cache_control() -> String {
    DEFAULT_CACHE_CONTROL.to_owned()
}

fn default_cache_dir() -> PathBuf {
    PathBuf::from("./cache")
}

const fn default_max_input_bytes() -> usize {
    DEFAULT_MAX_INPUT_BYTES
}

const fn default_max_source_dimension() -> u32 {
    DEFAULT_MAX_SOURCE_DIMENSION
}

const fn default_max_target_dimension() -> u32 {
    DEFAULT_MAX_DIMENSION
}

const fn default_max_decoded_pixels() -> u64 {
    DEFAULT_MAX_DECODED_PIXELS
}

const fn default_max_concurrent_transforms() -> usize {
    DEFAULT_MAX_CONCURRENT_TRANSFORMS
}

const fn default_true() -> bool {
    true
}

fn default_local_root() -> PathBuf {
    PathBuf::from("./assets")
}

const fn default_http_timeout_secs() -> u64 {
    15
}

const fn default_http_connect_timeout_secs() -> u64 {
    5
}

const fn default_max_redirects() -> u8 {
    DEFAULT_MAX_REDIRECTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_toml_parses_and_validates_cleanly() {
        let example = include_str!("../../murklet.example.toml");
        let cfg = MurkletConfig::from_toml_str(example).unwrap();

        assert_eq!(cfg.server.bind, SocketAddr::from(([127, 0, 0, 1], 9876)));
        assert!(cfg.origins.local.enabled);
        assert!(!cfg.origins.http.enabled);
        assert_eq!(cfg.server.cleanup_token, None);
    }

    #[test]
    fn empty_toml_uses_safe_defaults() {
        let cfg = MurkletConfig::from_toml_str("").unwrap();
        assert!(cfg.origins.local.enabled);
        assert!(!cfg.origins.http.enabled);
        assert_eq!(cfg.limits.max_target_dimension, DEFAULT_MAX_DIMENSION);
        assert_eq!(cfg.engine_limits(), EngineLimits::default());
        assert!(cfg.http_origin_config().is_none());
    }

    #[test]
    fn rejects_unknown_fields_so_typos_fail_fast() {
        let bad = r#"
[origins.http]
enabled = true
alowed_hosts = ["cdn.example.com"]
"#;
        assert!(matches!(
            MurkletConfig::from_toml_str(bad),
            Err(PolicyError::ParseToml(_))
        ));
    }

    #[test]
    fn enforces_security_and_resource_invariants() {
        // HTTP origin enabled without allowlist is rejected.
        let open_http = "
[origins.http]
enabled = true
allowed_hosts = []
";
        assert!(matches!(
            MurkletConfig::from_toml_str(open_http),
            Err(PolicyError::Validation(_))
        ));

        // All origins disabled is rejected.
        let no_origins = "
[origins.local]
enabled = false

[origins.http]
enabled = false
";
        assert!(matches!(
            MurkletConfig::from_toml_str(no_origins),
            Err(PolicyError::Validation(_))
        ));

        // Connect timeout exceeding total timeout is rejected.
        let bad_timeouts = r#"
[origins.http]
enabled = true
allowed_hosts = ["cdn.example.com"]
timeout_secs = 5
connect_timeout_secs = 10
"#;
        assert!(matches!(
            MurkletConfig::from_toml_str(bad_timeouts),
            Err(PolicyError::Validation(_))
        ));

        // Zero decoded pixel limit is rejected.
        let zero_pixels = "
[limits]
max_decoded_pixels = 0
";
        assert!(matches!(
            MurkletConfig::from_toml_str(zero_pixels),
            Err(PolicyError::Validation(_))
        ));
    }

    #[test]
    fn builds_http_origin_config_and_normalises_blank_cleanup_token() {
        let raw = r#"
[server]
cleanup_token = ""
diagnostic_headers = true

[limits]
animation_policy = "passthrough"

[origins.local]
enabled = false

[origins.http]
enabled = true
allowed_hosts = ["cdn.example.com", "*.assets.example.org"]
timeout_secs = 20
connect_timeout_secs = 4
max_redirects = 3
"#;
        let cfg = MurkletConfig::from_toml_str(raw).unwrap();
        assert_eq!(cfg.server.cleanup_token, None);
        assert!(cfg.server.diagnostic_headers);
        assert_eq!(
            cfg.engine_limits().animation_policy,
            AnimationPolicy::Passthrough
        );

        let http_cfg = cfg.http_origin_config().unwrap();
        assert!(http_cfg.allowlist.is_allowed("cdn.example.com"));
        assert!(http_cfg.allowlist.is_allowed("img.assets.example.org"));
        assert_eq!(http_cfg.timeout, Duration::from_secs(20));
        assert_eq!(http_cfg.connect_timeout, Duration::from_secs(4));
        assert_eq!(http_cfg.max_redirects, 3);
    }
}

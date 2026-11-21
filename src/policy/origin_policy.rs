//! Declarative per-origin transformation and delivery policies.
//!
//! Each configured origin (`local`, `http`, `s3`) may define its own
//! `[origins.<kind>.policy]` table to isolate security boundaries, lock public
//! routes to named presets (`presets_only`), restrict output formats, cap
//! dimensions, apply a default watermark, or override `Cache-Control`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;

use crate::domain::{Gravity, OutputFormat, WatermarkOpacity, WatermarkScale, WatermarkSpec};
use crate::policy::error::PolicyError;
use crate::policy::presets::PresetRegistry;

/// Raw TOML configuration for a single origin's policy (`[origins.<kind>.policy]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginPolicyConfig {
    /// When `true`, requests to this origin must specify `?preset=<name>` and must not
    /// supply ad-hoc transformation query parameters.
    #[serde(default)]
    pub presets_only: bool,
    /// Optional allowlist of preset names permitted for this origin (empty = all presets).
    #[serde(default)]
    pub allowed_presets: Vec<String>,
    /// Optional allowlist of output formats permitted for this origin (empty = all formats).
    #[serde(default)]
    pub allowed_formats: Vec<String>,
    /// Optional tighter per-origin maximum target width or height (`1..=limits.max_target_dimension`).
    #[serde(default)]
    pub max_target_dimension: Option<u32>,
    /// Optional logical watermark name from `[watermarks]` applied when none is requested.
    #[serde(default)]
    pub default_watermark: Option<String>,
    /// Optional per-origin `Cache-Control` response header override.
    #[serde(default)]
    pub cache_control: Option<String>,
}

/// Validated, immutable per-origin transformation and delivery policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OriginPolicy {
    /// Whether only named presets (without ad-hoc overrides) are permitted.
    pub presets_only: bool,
    /// Explicit set of permitted preset names (empty means all declared presets are allowed).
    pub allowed_presets: BTreeSet<String>,
    /// Explicit list of permitted output formats (empty means all formats are allowed).
    pub allowed_formats: Vec<OutputFormat>,
    /// Optional tighter per-origin maximum width/height ceiling.
    pub max_target_dimension: Option<u32>,
    /// Optional default watermark name applied when neither request nor preset sets one.
    pub default_watermark: Option<String>,
    /// Optional per-origin `Cache-Control` response header override.
    pub cache_control: Option<String>,
}

impl OriginPolicy {
    /// Validates raw TOML policy fields against global limits, declared presets, and watermarks.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::Validation`] if any constraint references an unknown preset or
    /// watermark, specifies an unsupported format, or violates dimension/header invariants.
    pub fn from_config(
        origin_label: &str,
        cfg: &OriginPolicyConfig,
        global_max_dimension: u32,
        presets: &PresetRegistry,
        watermarks: &BTreeMap<String, PathBuf>,
    ) -> Result<Self, PolicyError> {
        let mut allowed_presets = BTreeSet::new();
        for name in &cfg.allowed_presets {
            if presets.get(name).is_none() {
                return Err(PolicyError::Validation(format!(
                    "origins.{origin_label}.policy.allowed_presets references unknown preset '{name}'"
                )));
            }
            allowed_presets.insert(name.clone());
        }

        if cfg.presets_only && presets.is_empty() {
            return Err(PolicyError::Validation(format!(
                "origins.{origin_label}.policy.presets_only is true, but no [presets.*] are configured"
            )));
        }

        let mut allowed_formats = Vec::with_capacity(cfg.allowed_formats.len());
        for raw in &cfg.allowed_formats {
            let parsed = raw.parse::<OutputFormat>().map_err(|err| {
                PolicyError::Validation(format!(
                    "origins.{origin_label}.policy.allowed_formats: {err}"
                ))
            })?;
            if !allowed_formats.contains(&parsed) {
                allowed_formats.push(parsed);
            }
        }

        if let Some(max_dim) = cfg.max_target_dimension
            && (max_dim == 0 || max_dim > global_max_dimension)
        {
            return Err(PolicyError::Validation(format!(
                "origins.{origin_label}.policy.max_target_dimension ({max_dim}) must be between 1 and limits.max_target_dimension ({global_max_dimension})"
            )));
        }

        let default_watermark = cfg
            .default_watermark
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(|name| {
                if !watermarks.contains_key(name) {
                    return Err(PolicyError::Validation(format!(
                        "origins.{origin_label}.policy.default_watermark references undeclared watermark '{name}'"
                    )));
                }
                Ok(name.to_owned())
            })
            .transpose()?;

        let cache_control = cfg
            .cache_control
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(|value| {
                if value.trim().is_empty() || !value.is_ascii() || value.contains(['\r', '\n']) {
                    return Err(PolicyError::Validation(format!(
                        "origins.{origin_label}.policy.cache_control must be a non-empty single-line ASCII value"
                    )));
                }
                Ok(value.to_owned())
            })
            .transpose()?;

        Ok(Self {
            presets_only: cfg.presets_only,
            allowed_presets,
            allowed_formats,
            max_target_dimension: cfg.max_target_dimension,
            default_watermark,
            cache_control,
        })
    }

    /// Returns the effective maximum target dimension for this origin.
    #[must_use]
    pub fn effective_max_dimension(&self, global_max: u32) -> u32 {
        self.max_target_dimension
            .map_or(global_max, |limit| limit.min(global_max))
    }

    /// Reports whether `preset_name` is permitted by this origin's policy.
    #[must_use]
    pub fn is_preset_allowed(&self, preset_name: &str) -> bool {
        self.allowed_presets.is_empty() || self.allowed_presets.contains(preset_name)
    }

    /// Reports whether `format` is permitted by this origin's policy.
    #[must_use]
    pub fn is_format_allowed(&self, format: OutputFormat) -> bool {
        self.allowed_formats.is_empty() || self.allowed_formats.contains(&format)
    }

    /// Returns the effective `Cache-Control` header value for this origin.
    #[must_use]
    pub fn effective_cache_control<'a>(&'a self, global_default: &'a str) -> &'a str {
        self.cache_control.as_deref().unwrap_or(global_default)
    }

    /// Builds a default [`WatermarkSpec`] if this policy declares `default_watermark`.
    #[must_use]
    pub fn default_watermark_spec(&self) -> Option<WatermarkSpec> {
        self.default_watermark.as_deref().and_then(|name| {
            WatermarkSpec::new(
                name,
                Gravity::SouthEast,
                WatermarkOpacity::DEFAULT,
                WatermarkScale::DEFAULT,
            )
            .ok()
        })
    }

    /// Renders a concise human-readable summary for `murklet explain`.
    #[must_use]
    pub fn summary(&self, global_max_dim: u32, global_cache_control: &str) -> String {
        let formats = if self.allowed_formats.is_empty() {
            "all".to_owned()
        } else {
            self.allowed_formats
                .iter()
                .copied()
                .map(OutputFormat::as_str)
                .collect::<Vec<_>>()
                .join(",")
        };
        let presets = if self.allowed_presets.is_empty() {
            "all".to_owned()
        } else {
            self.allowed_presets
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "presets_only={} allowed_presets=[{}] allowed_formats=[{}] max_dim={} default_wm={} cache_control=\"{}\"",
            self.presets_only,
            presets,
            formats,
            self.effective_max_dimension(global_max_dim),
            self.default_watermark.as_deref().unwrap_or("none"),
            self.effective_cache_control(global_cache_control),
        )
    }
}

/// Collection of validated per-origin policies indexed by origin type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OriginPolicies {
    /// Policy governing `[origins.local]`.
    pub local: OriginPolicy,
    /// Policy governing `[origins.http]`.
    pub http: OriginPolicy,
    /// Policy governing `[origins.s3]`.
    pub s3: OriginPolicy,
}

impl OriginPolicies {
    /// Returns the [`OriginPolicy`] corresponding to a canonical origin ID (`"local"`, `"http"`, `"s3"`).
    #[must_use]
    pub fn for_origin_id(&self, origin_id: &str) -> &OriginPolicy {
        match origin_id {
            "http" => &self.http,
            "s3" => &self.s3,
            _ => &self.local,
        }
    }
}

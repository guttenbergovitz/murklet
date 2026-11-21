//! Validated named transformation presets declared in `murklet.toml`.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::domain::{Axis, Dimension, OutputFormat, Quality, ResizeFilter};
use crate::policy::error::PolicyError;

/// Raw TOML configuration for a single named preset (`[presets.<name>]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetConfig {
    /// Optional target width (`1..=max_target_dimension`).
    pub width: Option<u32>,
    /// Optional target height (`1..=max_target_dimension`).
    pub height: Option<u32>,
    /// Optional lossy output quality (`1..=100`).
    pub quality: Option<u8>,
    /// Optional output format (`jpeg`, `png`, `gif`, `webp`, `avif`).
    pub format: Option<String>,
    /// Optional resampling filter.
    pub filter: Option<String>,
}

/// A validated named preset whose fields already uphold all domain invariants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValidatedPreset {
    /// Optional target width.
    pub width: Option<Dimension>,
    /// Optional target height.
    pub height: Option<Dimension>,
    /// Optional lossy quality.
    pub quality: Option<Quality>,
    /// Optional output format.
    pub format: Option<OutputFormat>,
    /// Optional resampling filter.
    pub filter: Option<ResizeFilter>,
}

/// Immutable registry of validated named presets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresetRegistry {
    presets: BTreeMap<String, ValidatedPreset>,
}

impl PresetRegistry {
    /// Creates an empty preset registry.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            presets: BTreeMap::new(),
        }
    }

    /// Validates all raw preset entries against `max_target_dimension` and domain bounds.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::Validation`] if any preset name or field value is invalid.
    pub fn from_config(
        raw: &BTreeMap<String, PresetConfig>,
        max_target_dimension: u32,
    ) -> Result<Self, PolicyError> {
        let mut presets = BTreeMap::new();

        for (name, cfg) in raw {
            if !is_valid_preset_name(name) {
                return Err(PolicyError::Validation(format!(
                    "preset name '{name}' must be non-empty ASCII letters, digits, '-' or '_'"
                )));
            }

            let width = cfg
                .width
                .map(|v| Dimension::new(Axis::Width, v, max_target_dimension))
                .transpose()
                .map_err(|err| PolicyError::Validation(format!("preset '{name}': {err}")))?;

            let height = cfg
                .height
                .map(|v| Dimension::new(Axis::Height, v, max_target_dimension))
                .transpose()
                .map_err(|err| PolicyError::Validation(format!("preset '{name}': {err}")))?;

            let quality = cfg
                .quality
                .map(Quality::new)
                .transpose()
                .map_err(|err| PolicyError::Validation(format!("preset '{name}': {err}")))?;

            let format = cfg
                .format
                .as_deref()
                .map(str::parse::<OutputFormat>)
                .transpose()
                .map_err(|err| PolicyError::Validation(format!("preset '{name}': {err}")))?;

            let filter = cfg
                .filter
                .as_deref()
                .map(str::parse::<ResizeFilter>)
                .transpose()
                .map_err(|err| PolicyError::Validation(format!("preset '{name}': {err}")))?;

            presets.insert(
                name.clone(),
                ValidatedPreset {
                    width,
                    height,
                    quality,
                    format,
                    filter,
                },
            );
        }

        Ok(Self { presets })
    }

    /// Looks up a validated preset by exact name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ValidatedPreset> {
        self.presets.get(name)
    }

    /// Returns the number of configured presets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.presets.len()
    }

    /// Reports whether no presets are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.presets.is_empty()
    }
}

fn is_valid_preset_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DEFAULT_MAX_DIMENSION;

    #[test]
    fn validates_presets_and_rejects_invalid_names_or_bounds() {
        let mut raw = BTreeMap::new();
        raw.insert(
            "thumb".to_owned(),
            PresetConfig {
                width: Some(300),
                height: None,
                quality: Some(80),
                format: Some("webp".to_owned()),
                filter: Some("lanczos".to_owned()),
            },
        );

        let registry = PresetRegistry::from_config(&raw, DEFAULT_MAX_DIMENSION).unwrap();
        let thumb = registry.get("thumb").unwrap();
        assert_eq!(thumb.width.unwrap().get(), 300);
        assert_eq!(thumb.quality.unwrap().get(), 80);
        assert_eq!(thumb.format, Some(OutputFormat::Webp));
        assert_eq!(thumb.filter, Some(ResizeFilter::Lanczos));

        let mut bad_name = BTreeMap::new();
        bad_name.insert("bad name!".to_owned(), PresetConfig::default());
        assert!(matches!(
            PresetRegistry::from_config(&bad_name, DEFAULT_MAX_DIMENSION),
            Err(PolicyError::Validation(_))
        ));

        let mut bad_dim = BTreeMap::new();
        bad_dim.insert(
            "huge".to_owned(),
            PresetConfig {
                width: Some(99_999),
                ..PresetConfig::default()
            },
        );
        assert!(matches!(
            PresetRegistry::from_config(&bad_dim, DEFAULT_MAX_DIMENSION),
            Err(PolicyError::Validation(_))
        ));
    }
}

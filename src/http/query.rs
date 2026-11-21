//! Strict query-string parsing for `GET /load` into validated domain types.

use serde::Deserialize;

use crate::domain::{
    Axis, BackgroundColour, Dimension, Dpr, FitMode, Gravity, OutputFormat, Quality, ResizeFilter,
    Rotation, TargetDimensions, TransformSpec,
};
use crate::http::error::HttpError;
use crate::policy::{PresetRegistry, ValidatedPreset};

/// Raw query parameters accepted by `GET /load` before domain validation.
#[derive(Debug, Default, Deserialize)]
pub struct RawLoadQuery {
    /// Source asset identifier or URL.
    pub url: Option<String>,
    /// Optional named preset declared in `[presets.<name>]`.
    pub preset: Option<String>,
    /// Optional target width (`0` means unset; `1..=max` sets target width).
    pub w: Option<String>,
    /// Optional target height (`0` means unset; `1..=max` sets target height).
    pub h: Option<String>,
    /// Optional lossy quality (`1..=100`, default `75`).
    pub q: Option<String>,
    /// Optional output format (`jpg`, `jpeg`, `png`, `gif`, `webp`, `avif`; default `jpeg`).
    pub f: Option<String>,
    /// Optional resampling filter (`nearest`, `box`, `linear`, `hermite`, `catmullrom`, `bspline`, `gaussian`, `lanczos`).
    pub filter: Option<String>,
    /// Optional fit mode (`fill`, `contain`, `cover`, `scale-down`).
    pub fit: Option<String>,
    /// Optional crop gravity or focal point (`center`, `north`, `fp:0.5:0.2`, etc.).
    pub gravity: Option<String>,
    /// Optional clockwise rotation (`0`, `90`, `180`, `270`).
    pub rotate: Option<String>,
    /// Optional device pixel ratio (`1.0`..=`3.0`).
    pub dpr: Option<String>,
    /// Optional RGB hex background colour for alpha flattening (for example `ffffff`).
    pub bg: Option<String>,
}

/// Validated `GET /load` request ready for cache lookup and origin fetching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLoadRequest {
    /// Validated non-empty source reference.
    pub source: String,
    /// Optional matched preset name.
    pub preset: Option<String>,
    /// Canonical transformation specification.
    pub spec: TransformSpec,
    /// `true` when the client (or matched preset) explicitly specified an output format.
    pub explicit_format: bool,
}

/// Parses and strictly validates raw query parameters without named presets.
///
/// # Errors
///
/// Returns [`HttpError::BadRequest`] if `url` is missing/empty or any parameter
/// is malformed or outside permitted bounds.
pub fn parse_load_query(
    raw: RawLoadQuery,
    max_dimension: u32,
) -> Result<ParsedLoadRequest, HttpError> {
    parse_load_query_with_presets(raw, max_dimension, &PresetRegistry::empty())
}

/// Parses and strictly validates raw query parameters, applying optional named preset
/// defaults before explicit query-parameter overrides.
///
/// # Errors
///
/// Returns [`HttpError::BadRequest`] if `url` is missing/empty, `preset` is unknown,
/// or any parameter is malformed or outside permitted bounds.
pub fn parse_load_query_with_presets(
    raw: RawLoadQuery,
    max_dimension: u32,
    presets: &PresetRegistry,
) -> Result<ParsedLoadRequest, HttpError> {
    let source = raw
        .url
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HttpError::BadRequest("query parameter 'url' is required".to_owned()))?;

    let (preset_name, preset_defaults): (Option<String>, ValidatedPreset) =
        match raw.preset.filter(|value| !value.is_empty()) {
            None => (None, ValidatedPreset::default()),
            Some(name) => {
                let found = presets
                    .get(&name)
                    .copied()
                    .ok_or_else(|| HttpError::BadRequest(format!("unknown preset '{name}'")))?;
                (Some(name), found)
            }
        };

    let dpr = match raw.dpr.as_deref() {
        None | Some("") => preset_defaults.dpr.unwrap_or_default(),
        Some(value) => value.parse::<Dpr>()?,
    };

    let base_width = parse_optional_dimension(raw.w.as_deref(), Axis::Width, max_dimension)?
        .or(preset_defaults.width);
    let base_height = parse_optional_dimension(raw.h.as_deref(), Axis::Height, max_dimension)?
        .or(preset_defaults.height);

    let width = apply_dpr_to_dimension(base_width, dpr, Axis::Width, max_dimension)?;
    let height = apply_dpr_to_dimension(base_height, dpr, Axis::Height, max_dimension)?;

    let quality = match raw.q.as_deref() {
        None | Some("") => preset_defaults.quality.unwrap_or_default(),
        Some(value) => {
            let parsed = value.parse::<u8>().map_err(|_| {
                HttpError::BadRequest(format!(
                    "query parameter 'q' must be an integer between 1 and 100, got '{value}'"
                ))
            })?;
            Quality::new(parsed)?
        }
    };

    let (format, explicit_format) = match raw.f.as_deref() {
        None | Some("") => match preset_defaults.format {
            Some(preset_fmt) => (preset_fmt, true),
            None => (OutputFormat::default(), false),
        },
        Some(value) => (value.parse::<OutputFormat>()?, true),
    };

    let filter = match raw.filter.as_deref() {
        None | Some("") => preset_defaults.filter,
        Some(value) => Some(value.parse::<ResizeFilter>()?),
    };

    let fit = match raw.fit.as_deref() {
        None | Some("") => preset_defaults.fit.unwrap_or_default(),
        Some(value) => value.parse::<FitMode>()?,
    };

    let gravity = match raw.gravity.as_deref() {
        None | Some("") => preset_defaults.gravity.unwrap_or_default(),
        Some(value) => value.parse::<Gravity>()?,
    };

    let rotation = match raw.rotate.as_deref() {
        None | Some("") => preset_defaults.rotation.unwrap_or_default(),
        Some(value) => {
            let degrees = value.parse::<u16>().map_err(|_| {
                HttpError::BadRequest(format!(
                    "query parameter 'rotate' must be 0, 90, 180, or 270, got '{value}'"
                ))
            })?;
            Rotation::from_degrees(degrees)?
        }
    };

    let background = match raw.bg.as_deref() {
        None | Some("") => preset_defaults.background,
        Some(value) => Some(value.parse::<BackgroundColour>()?),
    };

    Ok(ParsedLoadRequest {
        source,
        preset: preset_name,
        spec: TransformSpec {
            dimensions: TargetDimensions::new(width, height),
            format,
            quality,
            filter,
            fit,
            gravity,
            rotation,
            background,
        },
        explicit_format,
    })
}

fn apply_dpr_to_dimension(
    dim: Option<Dimension>,
    dpr: Dpr,
    axis: Axis,
    max_dimension: u32,
) -> Result<Option<Dimension>, HttpError> {
    let Some(base) = dim else {
        return Ok(None);
    };
    if dpr == Dpr::ONE {
        return Ok(Some(base));
    }
    let scaled = dpr.scale_dimension(base.get());
    Ok(Some(Dimension::new(axis, scaled, max_dimension)?))
}

fn parse_optional_dimension(
    raw: Option<&str>,
    axis: Axis,
    max_dimension: u32,
) -> Result<Option<Dimension>, HttpError> {
    let Some(value) = raw.filter(|v| !v.is_empty()) else {
        return Ok(None);
    };

    let parsed = value.parse::<u32>().map_err(|_| {
        HttpError::BadRequest(format!(
            "{axis} must be an integer between 0 and {max_dimension}, got '{value}'"
        ))
    })?;

    if parsed == 0 {
        return Ok(None);
    }

    Ok(Some(Dimension::new(axis, parsed, max_dimension)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DEFAULT_MAX_DIMENSION;

    #[test]
    fn parses_valid_query_with_defaults_and_overrides() {
        let parsed = parse_load_query(
            RawLoadQuery {
                url: Some("products/hero.png".to_owned()),
                preset: None,
                w: Some("800".to_owned()),
                h: Some("0".to_owned()),
                q: Some("85".to_owned()),
                f: Some("WebP".to_owned()),
                filter: Some("lanczos".to_owned()),
                ..RawLoadQuery::default()
            },
            DEFAULT_MAX_DIMENSION,
        )
        .unwrap();

        assert_eq!(parsed.source, "products/hero.png");
        assert_eq!(parsed.spec.dimensions.width().unwrap().get(), 800);
        assert_eq!(parsed.spec.dimensions.height(), None);
        assert_eq!(parsed.spec.quality.get(), 85);
        assert_eq!(parsed.spec.format, OutputFormat::Webp);
        assert_eq!(parsed.spec.filter, Some(ResizeFilter::Lanczos));
    }

    #[test]
    fn rejects_missing_url_and_malformed_parameters() {
        assert!(matches!(
            parse_load_query(RawLoadQuery::default(), DEFAULT_MAX_DIMENSION),
            Err(HttpError::BadRequest(_))
        ));

        for bad in [
            RawLoadQuery {
                url: Some("a.png".to_owned()),
                w: Some("abc".to_owned()),
                ..RawLoadQuery::default()
            },
            RawLoadQuery {
                url: Some("a.png".to_owned()),
                w: Some("99999".to_owned()),
                ..RawLoadQuery::default()
            },
            RawLoadQuery {
                url: Some("a.png".to_owned()),
                q: Some("0".to_owned()),
                ..RawLoadQuery::default()
            },
            RawLoadQuery {
                url: Some("a.png".to_owned()),
                q: Some("101".to_owned()),
                ..RawLoadQuery::default()
            },
            RawLoadQuery {
                url: Some("a.png".to_owned()),
                f: Some("bmp".to_owned()),
                ..RawLoadQuery::default()
            },
            RawLoadQuery {
                url: Some("a.png".to_owned()),
                filter: Some("magic".to_owned()),
                ..RawLoadQuery::default()
            },
        ] {
            assert!(matches!(
                parse_load_query(bad, DEFAULT_MAX_DIMENSION),
                Err(HttpError::BadRequest(_))
            ));
        }
    }
}

//! Strict query-string parsing for `GET /load` into validated domain types.

use serde::Deserialize;

use crate::domain::{
    Axis, BackgroundColour, BlurSigma, Brightness, Contrast, Dimension, DomainError, Dpr, Duotone,
    FitMode, Flip, Gamma, Gravity, Hue, ImageFilters, OutputFormat, Pixelate, Posterize, Quality,
    ResizeFilter, Rotation, Saturation, Sepia, SharpenSigma, TargetDimensions, TransformSpec,
    Vignette, WatermarkOpacity, WatermarkScale, WatermarkSpec,
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
    /// Optional mirror flip (`h`, `v`, `hv`).
    pub flip: Option<String>,
    /// Optional greyscale conversion (`true`/`false`/`1`/`0`).
    #[serde(alias = "greyscale")]
    pub grayscale: Option<String>,
    /// Optional negative / colour inversion (`true`/`false`/`1`/`0`).
    #[serde(alias = "negative")]
    pub invert: Option<String>,
    /// Optional blur sigma (`0.5..=20.0`).
    pub blur: Option<String>,
    /// Optional sharpen sigma (`0.5..=10.0`).
    pub sharpen: Option<String>,
    /// Optional brightness adjustment (`-100..=100`).
    pub brightness: Option<String>,
    /// Optional contrast adjustment (`-100..=100`).
    pub contrast: Option<String>,
    /// Optional saturation adjustment (`-100..=100`).
    pub saturation: Option<String>,
    /// Optional hue rotation (`-360..=360`).
    pub hue: Option<String>,
    /// Optional tonal gamma (`0.2..=5.0`).
    pub gamma: Option<String>,
    /// Optional sepia intensity (`0..=100`, or `true`/`false`).
    pub sepia: Option<String>,
    /// Optional brand monochrome tint colour (3- or 6-digit RGB hex).
    #[serde(alias = "mono", alias = "monochrome")]
    pub tint: Option<String>,
    /// Optional duotone colour pair (`<dark_hex>:<light_hex>`).
    pub duotone: Option<String>,
    /// Optional posterize level count (`2..=16`).
    pub posterize: Option<String>,
    /// Optional binary luminance threshold (`0..=255`).
    pub threshold: Option<String>,
    /// Optional block pixelation size (`2..=64`).
    pub pixelate: Option<String>,
    /// Optional radial vignette strength (`0..=100`).
    pub vignette: Option<String>,
    /// Optional logical watermark name declared in `[watermarks]`.
    #[serde(alias = "watermark")]
    pub wm: Option<String>,
    /// Optional watermark anchor position (`south-east`, `center`, `fp:0.5:0.5`, etc.).
    pub wm_pos: Option<String>,
    /// Optional watermark opacity percentage (`1..=100`, default `50`).
    pub wm_opacity: Option<String>,
    /// Optional watermark width scale percentage (`5..=100`, default `20`).
    pub wm_scale: Option<String>,
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
    let default_preset = ValidatedPreset::default();
    let (preset_name, preset_defaults): (Option<String>, &ValidatedPreset) =
        match raw.preset.as_deref().filter(|value| !value.is_empty()) {
            None => (None, &default_preset),
            Some(name) => {
                let found = presets
                    .get(name)
                    .ok_or_else(|| HttpError::BadRequest(format!("unknown preset '{name}'")))?;
                (Some(name.to_owned()), found)
            }
        };

    let filters = parse_image_filters(&raw, preset_defaults)?;
    let watermark = parse_watermark(&raw, preset_defaults)?;

    let source = raw
        .url
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HttpError::BadRequest("query parameter 'url' is required".to_owned()))?;

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
            filters,
            watermark,
        },
        explicit_format,
    })
}

fn parse_watermark(
    raw: &RawLoadQuery,
    preset: &ValidatedPreset,
) -> Result<Option<WatermarkSpec>, HttpError> {
    let position = match raw.wm_pos.as_deref() {
        None | Some("") => preset.wm_pos.unwrap_or(Gravity::SouthEast),
        Some(value) => value.parse::<Gravity>()?,
    };
    let opacity = match raw.wm_opacity.as_deref() {
        None | Some("") => preset.wm_opacity.unwrap_or_default(),
        Some(value) => WatermarkOpacity::new(parse_u8_param("wm_opacity", value, 1, 100)?)?,
    };
    let scale = match raw.wm_scale.as_deref() {
        None | Some("") => preset.wm_scale.unwrap_or_default(),
        Some(value) => WatermarkScale::new(parse_u8_param("wm_scale", value, 5, 100)?)?,
    };

    let wm_name = match raw.wm.as_deref().filter(|v| !v.is_empty()) {
        Some(name) => Some(name),
        None => preset.watermark.as_deref(),
    };

    wm_name
        .map(|name| WatermarkSpec::new(name, position, opacity, scale))
        .transpose()
        .map_err(HttpError::from)
}

fn parse_image_filters(
    raw: &RawLoadQuery,
    preset: &ValidatedPreset,
) -> Result<ImageFilters, HttpError> {
    let flip = match raw.flip.as_deref() {
        None | Some("") => preset.flip.unwrap_or_default(),
        Some(value) => value.parse::<Flip>()?,
    };
    let grayscale = match raw.grayscale.as_deref() {
        None | Some("") => preset.grayscale.unwrap_or(false),
        Some(value) => parse_bool_param("grayscale", value)?,
    };
    let invert = match raw.invert.as_deref() {
        None | Some("") => preset.invert.unwrap_or(false),
        Some(value) => parse_bool_param("invert", value)?,
    };
    let blur = match raw.blur.as_deref() {
        None | Some("") => preset.blur,
        Some(value) => Some(value.parse::<BlurSigma>()?),
    };
    let sharpen = match raw.sharpen.as_deref() {
        None | Some("") => preset.sharpen,
        Some(value) => Some(value.parse::<SharpenSigma>()?),
    };
    let brightness = match raw.brightness.as_deref() {
        None | Some("") => preset.brightness.unwrap_or_default(),
        Some(value) => Brightness::new(parse_signed_param("brightness", value, -100, 100)?)?,
    };
    let contrast = match raw.contrast.as_deref() {
        None | Some("") => preset.contrast.unwrap_or_default(),
        Some(value) => Contrast::new(parse_signed_param("contrast", value, -100, 100)?)?,
    };
    let saturation = match raw.saturation.as_deref() {
        None | Some("") => preset.saturation.unwrap_or_default(),
        Some(value) => Saturation::new(parse_signed_param("saturation", value, -100, 100)?)?,
    };
    let hue = match raw.hue.as_deref() {
        None | Some("") => preset.hue.unwrap_or_default(),
        Some(value) => {
            let parsed = value.parse::<i32>().map_err(|_| {
                HttpError::BadRequest(format!(
                    "query parameter 'hue' must be an integer between -360 and 360, got '{value}'"
                ))
            })?;
            Hue::new(parsed)?
        }
    };
    let gamma = match raw.gamma.as_deref() {
        None | Some("") => preset.gamma.unwrap_or_default(),
        Some(value) => value.parse::<Gamma>()?,
    };
    let sepia = match raw.sepia.as_deref() {
        None | Some("") => preset.sepia.unwrap_or_default(),
        Some(value) => value.parse::<Sepia>()?,
    };
    let tint = match raw.tint.as_deref() {
        None | Some("") => preset.tint,
        Some(value) => Some(
            value
                .parse::<BackgroundColour>()
                .map_err(|_| DomainError::InvalidTint(value.to_owned()))?,
        ),
    };

    let mut filters = ImageFilters {
        flip,
        grayscale,
        invert,
        blur,
        sharpen,
        brightness,
        contrast,
        saturation,
        hue,
        gamma,
        sepia,
        tint,
        ..ImageFilters::default()
    };
    populate_creative_filters(raw, preset, &mut filters)?;
    Ok(filters)
}

fn populate_creative_filters(
    raw: &RawLoadQuery,
    preset: &ValidatedPreset,
    filters: &mut ImageFilters,
) -> Result<(), HttpError> {
    filters.duotone = match raw.duotone.as_deref() {
        None | Some("") => preset.duotone,
        Some(value) => Some(value.parse::<Duotone>()?),
    };
    filters.posterize = match raw.posterize.as_deref() {
        None | Some("") => preset.posterize,
        Some(value) => Some(Posterize::new(parse_u8_param("posterize", value, 2, 16)?)?),
    };
    filters.threshold = match raw.threshold.as_deref() {
        None | Some("") => preset.threshold,
        Some(value) => Some(parse_u8_param("threshold", value, 0, 255)?),
    };
    filters.pixelate = match raw.pixelate.as_deref() {
        None | Some("") => preset.pixelate,
        Some(value) => Some(Pixelate::new(parse_u8_param("pixelate", value, 2, 64)?)?),
    };
    filters.vignette = match raw.vignette.as_deref() {
        None | Some("") => preset.vignette.unwrap_or_default(),
        Some(value) => Vignette::new(parse_u8_param("vignette", value, 0, 100)?)?,
    };
    Ok(())
}

fn parse_u8_param(name: &str, raw: &str, min: u8, max: u8) -> Result<u8, HttpError> {
    raw.parse::<u8>().map_err(|_| {
        HttpError::BadRequest(format!(
            "query parameter '{name}' must be an integer between {min} and {max}, got '{raw}'"
        ))
    })
}

fn parse_signed_param(name: &str, raw: &str, min: i16, max: i16) -> Result<i16, HttpError> {
    raw.parse::<i16>().map_err(|_| {
        HttpError::BadRequest(format!(
            "query parameter '{name}' must be an integer between {min} and {max}, got '{raw}'"
        ))
    })
}

fn parse_bool_param(name: &str, raw: &str) -> Result<bool, HttpError> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(HttpError::BadRequest(format!(
            "query parameter '{name}' must be true, false, 1, or 0, got '{raw}'"
        ))),
    }
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

    #[test]
    fn parses_watermark_query_parameters_and_preset_overrides() {
        let mut raw_presets = std::collections::BTreeMap::new();
        raw_presets.insert(
            "branded".to_owned(),
            crate::policy::PresetConfig {
                watermark: Some("brand".to_owned()),
                wm_pos: Some("south-east".to_owned()),
                wm_opacity: Some(40),
                wm_scale: Some(25),
                ..crate::policy::PresetConfig::default()
            },
        );
        let presets = PresetRegistry::from_config(&raw_presets, DEFAULT_MAX_DIMENSION).unwrap();

        let from_preset = parse_load_query_with_presets(
            RawLoadQuery {
                url: Some("hero.jpg".to_owned()),
                preset: Some("branded".to_owned()),
                wm_pos: Some("center".to_owned()),
                ..RawLoadQuery::default()
            },
            DEFAULT_MAX_DIMENSION,
            &presets,
        )
        .unwrap();

        let wm = from_preset.spec.watermark.unwrap();
        assert_eq!(wm.name, "brand");
        assert_eq!(wm.position, Gravity::Center);
        assert_eq!(wm.opacity.get(), 40);
        assert_eq!(wm.scale.get(), 25);

        // Invalid watermark opacity / scale / name rejected.
        for bad in [
            RawLoadQuery {
                url: Some("a.png".to_owned()),
                wm: Some("../evil".to_owned()),
                ..RawLoadQuery::default()
            },
            RawLoadQuery {
                url: Some("a.png".to_owned()),
                wm: Some("brand".to_owned()),
                wm_opacity: Some("0".to_owned()),
                ..RawLoadQuery::default()
            },
            RawLoadQuery {
                url: Some("a.png".to_owned()),
                wm: Some("brand".to_owned()),
                wm_scale: Some("4".to_owned()),
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

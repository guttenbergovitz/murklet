//! Startup-decoded watermark registry and SIMD-assisted alpha compositing.
//!
//! Watermark assets are loaded from policy-declared local paths once at startup
//! and decoded under [`EngineLimits`] so runtime requests never touch the
//! filesystem or accept client-controlled paths.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use image::{DynamicImage, RgbaImage};

use crate::domain::{
    Gravity, ImageDimensions, ResizeFilter, WatermarkSpec, is_valid_watermark_name,
};
use crate::engine::error::EngineError;
use crate::engine::limits::EngineLimits;
use crate::engine::pipeline::{decode_with_orientation, inspect_header, map_resize_alg};

/// Edge margin percentage (`2%`) applied to non-centred watermark anchors.
const WATERMARK_MARGIN_PERCENT: u64 = 2;

/// Immutable registry of pre-decoded `RgbaImage` watermarks keyed by logical name.
#[derive(Debug, Clone, Default)]
pub struct WatermarkRegistry {
    watermarks: BTreeMap<String, Arc<RgbaImage>>,
}

impl WatermarkRegistry {
    /// Creates an empty watermark registry.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            watermarks: BTreeMap::new(),
        }
    }

    /// Loads, validates, and decodes all policy-declared watermarks from local disk.
    ///
    /// Every file is read bounded by `limits.max_input_bytes` and validated against
    /// `limits.max_source_dimension` and `limits.max_decoded_pixels` before full decode.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WatermarkLoadFailed`] if any name is invalid, a file
    /// cannot be read or is not a regular file, or decoding fails / exceeds limits.
    pub fn load_from_paths(
        paths: &BTreeMap<String, PathBuf>,
        limits: &EngineLimits,
    ) -> Result<Self, EngineError> {
        let mut watermarks = BTreeMap::new();

        for (name, path) in paths {
            if !is_valid_watermark_name(name) {
                return Err(EngineError::WatermarkLoadFailed {
                    name: name.clone(),
                    reason: "name must be non-empty ASCII letters, digits, '-' or '_'".to_owned(),
                });
            }

            let decoded = load_single_watermark(name, path, limits)?;
            watermarks.insert(name.clone(), Arc::new(decoded));
        }

        Ok(Self { watermarks })
    }

    /// Inserts a pre-decoded watermark image directly (primarily for library callers and tests).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::WatermarkLoadFailed`] if `name` is invalid or `image` has zero area.
    pub fn insert(&mut self, name: impl Into<String>, image: RgbaImage) -> Result<(), EngineError> {
        let name = name.into();
        if !is_valid_watermark_name(&name) {
            return Err(EngineError::WatermarkLoadFailed {
                name,
                reason: "name must be non-empty ASCII letters, digits, '-' or '_'".to_owned(),
            });
        }
        if image.width() == 0 || image.height() == 0 {
            return Err(EngineError::WatermarkLoadFailed {
                name,
                reason: "watermark image dimensions must be non-zero".to_owned(),
            });
        }
        self.watermarks.insert(name, Arc::new(image));
        Ok(())
    }

    /// Looks up a pre-decoded watermark image by logical name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<RgbaImage>> {
        self.watermarks.get(name).map(Arc::clone)
    }

    /// Reports whether a logical watermark name is registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.watermarks.contains_key(name)
    }

    /// Returns the number of registered watermarks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.watermarks.len()
    }

    /// Reports whether no watermarks are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.watermarks.is_empty()
    }
}

fn load_single_watermark(
    name: &str,
    path: &Path,
    limits: &EngineLimits,
) -> Result<RgbaImage, EngineError> {
    let wrap = |reason: String| EngineError::WatermarkLoadFailed {
        name: name.to_owned(),
        reason,
    };

    let file = File::open(path).map_err(|err| wrap(err.to_string()))?;
    let meta = file.metadata().map_err(|err| wrap(err.to_string()))?;
    if !meta.is_file() {
        return Err(wrap("watermark path is not a regular file".to_owned()));
    }

    let max_bytes = limits.max_input_bytes;
    let max_read = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut raw = Vec::new();
    file.take(max_read)
        .read_to_end(&mut raw)
        .map_err(|err| wrap(err.to_string()))?;

    if raw.len() > max_bytes {
        return Err(wrap(format!(
            "file exceeds max_input_bytes ({max_bytes} bytes)"
        )));
    }

    let (format, _) = inspect_header(&raw, limits).map_err(|err| wrap(err.to_string()))?;
    let decoded =
        decode_with_orientation(&raw, format, limits).map_err(|err| wrap(err.to_string()))?;

    Ok(decoded.into_rgba8())
}

/// Resizes `watermark` proportionally to `spec.scale` of `image`'s width using SIMD
/// `fast_image_resize` and alpha-composites it at `spec.position` with `spec.opacity`.
///
/// # Errors
///
/// Returns [`EngineError::EncodeFailed`] if SIMD resizing fails unexpectedly.
pub(crate) fn apply_watermark(
    image: DynamicImage,
    watermark: &RgbaImage,
    spec: &WatermarkSpec,
) -> Result<DynamicImage, EngineError> {
    let (tw, th) = (image.width(), image.height());
    if tw == 0 || th == 0 || watermark.width() == 0 || watermark.height() == 0 {
        return Ok(image);
    }

    let (wm_w, wm_h) = compute_watermark_dimensions(
        tw,
        th,
        watermark.width(),
        watermark.height(),
        spec.scale.get(),
    );

    let resized: Cow<'_, RgbaImage> = if wm_w == watermark.width() && wm_h == watermark.height() {
        Cow::Borrowed(watermark)
    } else {
        Cow::Owned(resize_rgba(watermark, wm_w, wm_h)?)
    };

    let (offset_x, offset_y) = compute_watermark_origin(tw, th, wm_w, wm_h, spec.position);
    let opacity = u32::from(spec.opacity.get());

    if image.color().has_alpha() {
        let mut dst = image.into_rgba8();
        composite_over_rgba(&mut dst, &resized, offset_x, offset_y, opacity);
        Ok(DynamicImage::ImageRgba8(dst))
    } else {
        let mut dst = image.into_rgb8();
        composite_over_rgb(&mut dst, &resized, offset_x, offset_y, opacity);
        Ok(DynamicImage::ImageRgb8(dst))
    }
}

/// Computes target `(width, height)` for a watermark scaled to `scale_percent` of `target_w`,
/// preserving aspect ratio and clamping within `(target_w, target_h)`.
#[must_use]
fn compute_watermark_dimensions(
    target_w: u32,
    target_h: u32,
    wm_w: u32,
    wm_h: u32,
    scale_percent: u8,
) -> (u32, u32) {
    let tw = u64::from(target_w);
    let th = u64::from(target_h);
    let ww = u64::from(wm_w);
    let wh = u64::from(wm_h);

    let mut out_w = ((tw * u64::from(scale_percent) + 50) / 100).max(1).min(tw);
    let mut out_h = ((out_w * wh + ww / 2) / ww).max(1);

    if out_h > th {
        out_h = th;
        out_w = ((out_h * ww + wh / 2) / wh).max(1).min(tw);
    }

    (
        u32::try_from(out_w).unwrap_or(target_w),
        u32::try_from(out_h).unwrap_or(target_h),
    )
}

/// Computes top-left `(x, y)` placement for a watermark of `(wm_w, wm_h)` on `(target_w, target_h)`,
/// applying a 2% edge margin for non-centred named anchors.
#[must_use]
fn compute_watermark_origin(
    target_w: u32,
    target_h: u32,
    wm_w: u32,
    wm_h: u32,
    position: Gravity,
) -> (u32, u32) {
    let max_x = target_w.saturating_sub(wm_w);
    let max_y = target_h.saturating_sub(wm_h);

    if let Gravity::FocalPoint { x_milli, y_milli } = position {
        let focal_x = (u64::from(target_w) * u64::from(x_milli)) / 1000;
        let focal_y = (u64::from(target_h) * u64::from(y_milli)) / 1000;
        let desired_x = focal_x.saturating_sub(u64::from(wm_w) / 2);
        let desired_y = focal_y.saturating_sub(u64::from(wm_h) / 2);
        return (
            u32::try_from(desired_x).unwrap_or(max_x).min(max_x),
            u32::try_from(desired_y).unwrap_or(max_y).min(max_y),
        );
    }

    let margin_x = u32::try_from((u64::from(target_w) * WATERMARK_MARGIN_PERCENT) / 100)
        .unwrap_or(0)
        .min(max_x);
    let margin_y = u32::try_from((u64::from(target_h) * WATERMARK_MARGIN_PERCENT) / 100)
        .unwrap_or(0)
        .min(max_y);

    let x = match position {
        Gravity::West | Gravity::NorthWest | Gravity::SouthWest => margin_x,
        Gravity::Center | Gravity::North | Gravity::South => max_x / 2,
        Gravity::East | Gravity::NorthEast | Gravity::SouthEast => max_x.saturating_sub(margin_x),
        Gravity::FocalPoint { .. } => unreachable!(),
    };

    let y = match position {
        Gravity::North | Gravity::NorthWest | Gravity::NorthEast => margin_y,
        Gravity::Center | Gravity::East | Gravity::West => max_y / 2,
        Gravity::South | Gravity::SouthWest | Gravity::SouthEast => max_y.saturating_sub(margin_y),
        Gravity::FocalPoint { .. } => unreachable!(),
    };

    (x, y)
}

fn resize_rgba(src: &RgbaImage, dst_w: u32, dst_h: u32) -> Result<RgbaImage, EngineError> {
    let src_dims = ImageDimensions::new(src.width(), src.height())
        .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;
    let dst_dims = ImageDimensions::new(dst_w, dst_h)
        .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;
    let selected = ResizeFilter::select_for(None, src_dims, dst_dims);
    let options = ResizeOptions::new().resize_alg(map_resize_alg(selected.filter));

    let src_image = ImageRef::new(src.width(), src.height(), src.as_raw(), PixelType::U8x4)
        .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;
    let mut dst_image = Image::new(dst_w, dst_h, PixelType::U8x4);
    let mut resizer = Resizer::new();
    resizer
        .resize(&src_image, &mut dst_image, &options)
        .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;

    RgbaImage::from_raw(dst_w, dst_h, dst_image.into_vec()).ok_or_else(|| {
        EngineError::EncodeFailed("resized watermark RGBA buffer length mismatch".to_owned())
    })
}

/// Fast integer alpha-compositing onto an opaque `RgbImage`.
fn composite_over_rgb(
    dst: &mut image::RgbImage,
    wm: &RgbaImage,
    offset_x: u32,
    offset_y: u32,
    opacity_pct: u32,
) {
    for (wx, wy, wm_px) in wm.enumerate_pixels() {
        let [sr, sg, sb, sa] = wm_px.0;
        let eff_a = (u32::from(sa) * opacity_pct + 50) / 100;
        if eff_a == 0 {
            continue;
        }
        let inv_a = 255 - eff_a;
        let dst_px = dst.get_pixel_mut(offset_x + wx, offset_y + wy);
        let [dr, dg, db] = dst_px.0;

        let blend = |fg: u8, bg: u8| -> u8 {
            let mixed = (u32::from(fg) * eff_a + u32::from(bg) * inv_a + 127) / 255;
            u8::try_from(mixed).unwrap_or(255)
        };

        dst_px.0 = [blend(sr, dr), blend(sg, dg), blend(sb, db)];
    }
}

/// Porter-Duff `source-over` straight-alpha compositing onto an `RgbaImage`.
fn composite_over_rgba(
    dst: &mut RgbaImage,
    wm: &RgbaImage,
    offset_x: u32,
    offset_y: u32,
    opacity_pct: u32,
) {
    for (wx, wy, wm_px) in wm.enumerate_pixels() {
        let [sr, sg, sb, sa] = wm_px.0;
        let eff_a = (u32::from(sa) * opacity_pct + 50) / 100;
        if eff_a == 0 {
            continue;
        }

        let dst_px = dst.get_pixel_mut(offset_x + wx, offset_y + wy);
        let [dr, dg, db, da] = dst_px.0;

        if da == 255 {
            let inv_a = 255 - eff_a;
            let blend = |fg: u8, bg: u8| -> u8 {
                let mixed = (u32::from(fg) * eff_a + u32::from(bg) * inv_a + 127) / 255;
                u8::try_from(mixed).unwrap_or(255)
            };
            dst_px.0 = [blend(sr, dr), blend(sg, dg), blend(sb, db), 255];
        } else {
            let sa_f = f32::from(u8::try_from(eff_a).unwrap_or(255)) / 255.0;
            let da_f = f32::from(da) / 255.0;
            let inv_sa = 1.0 - sa_f;
            let out_a = sa_f + da_f * inv_sa;
            if out_a <= 0.0 {
                continue;
            }
            let blend = |fg: u8, bg: u8| -> u8 {
                let c = (f32::from(fg) * sa_f + f32::from(bg) * da_f * inv_sa) / out_a;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    c.round().clamp(0.0, 255.0) as u8
                }
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let out_a_u8 = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
            dst_px.0 = [blend(sr, dr), blend(sg, dg), blend(sb, db), out_a_u8];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{WatermarkOpacity, WatermarkScale};
    use image::{Rgb, RgbImage, Rgba};

    #[test]
    fn computes_watermark_dimensions_and_anchor_margins() {
        assert_eq!(compute_watermark_dimensions(200, 100, 50, 25, 20), (40, 20));
        // Tall watermark on short target clamps height to target height.
        assert_eq!(compute_watermark_dimensions(200, 20, 40, 80, 50), (10, 20));

        // 2% margin on 100x100 target is 2px.
        assert_eq!(
            compute_watermark_origin(100, 100, 20, 10, Gravity::NorthWest),
            (2, 2)
        );
        assert_eq!(
            compute_watermark_origin(100, 100, 20, 10, Gravity::SouthEast),
            (78, 88)
        );
        assert_eq!(
            compute_watermark_origin(100, 100, 20, 10, Gravity::Center),
            (40, 45)
        );
    }

    #[test]
    fn composites_watermark_with_opacity_onto_rgb_and_rgba() {
        let base = DynamicImage::ImageRgb8(RgbImage::from_pixel(100, 100, Rgb([0, 0, 0])));
        let wm = RgbaImage::from_pixel(20, 20, Rgba([200, 100, 50, 255]));
        let spec = WatermarkSpec::new(
            "brand",
            Gravity::Center,
            WatermarkOpacity::new(50).unwrap(),
            WatermarkScale::new(20).unwrap(),
        )
        .unwrap();

        let out = apply_watermark(base, &wm, &spec).unwrap().into_rgb8();
        // Corner remains untouched black.
        assert_eq!(out.get_pixel(0, 0).0, [0, 0, 0]);
        // Centre pixel is ~50% blended with [200, 100, 50].
        let center = out.get_pixel(50, 50).0;
        assert!((i16::from(center[0]) - 100).abs() <= 2);
        assert!((i16::from(center[1]) - 50).abs() <= 2);
        assert!((i16::from(center[2]) - 25).abs() <= 2);
    }
}

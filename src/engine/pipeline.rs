//! Pure-Rust image transformation pipeline: validate -> decode -> resize -> encode.

use std::io::Cursor;

use fast_image_resize::images::Image;
use fast_image_resize::{
    FilterType as FirFilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
};
use image::codecs::avif::AvifEncoder;
use image::codecs::gif::GifDecoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::webp::WebPDecoder;
use image::metadata::Orientation;
use image::{
    AnimationDecoder, DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat,
    ImageReader, Limits, RgbImage, RgbaImage,
};

use crate::domain::{
    BackgroundColour, Duotone, Flip, ImageDimensions, ImageFilters, OutputFormat, ResizeFilter,
    Rotation, SelectedFilter, TransformSpec, plan_resize,
};
use crate::engine::error::EngineError;
use crate::engine::limits::{AnimationPolicy, EngineLimits};

/// Default `rav1e` speed preset (`1..=10`; higher is faster) tuned for on-demand HTTP delivery.
const DEFAULT_AVIF_SPEED: u8 = 8;

/// Encoded output and metadata returned by [`transform`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformOutput {
    /// Final encoded image bytes ready for caching or HTTP delivery.
    pub bytes: Vec<u8>,
    /// Actual output format of `bytes`.
    pub format: OutputFormat,
    /// Source dimensions after applying EXIF orientation.
    pub source_dimensions: ImageDimensions,
    /// Final dimensions of the encoded image.
    pub output_dimensions: ImageDimensions,
    /// Resampling filter selected for this request.
    pub filter: SelectedFilter,
    /// `true` when an animated source was passed through unchanged under [`AnimationPolicy::Passthrough`].
    pub was_passthrough: bool,
}

/// Validates, decodes, transforms, and encodes raw image bytes using a pure-Rust pipeline.
///
/// Enforces byte, dimension, and pixel limits from image headers before allocating
/// full pixel buffers so malformed or oversized payloads fail fast.
///
/// # Errors
///
/// Returns an [`EngineError`] if input limits are exceeded, the format is unsupported,
/// or decoding/encoding fails.
pub fn transform(
    input: &[u8],
    spec: &TransformSpec,
    limits: &EngineLimits,
) -> Result<TransformOutput, EngineError> {
    if input.len() > limits.max_input_bytes {
        return Err(EngineError::InputTooLarge {
            size: input.len(),
            max: limits.max_input_bytes,
        });
    }

    let (input_format, header_dimensions) = inspect_header(input, limits)?;

    if limits.animation_policy == AnimationPolicy::Passthrough
        && let Some(passthrough_format) = detect_animated_format(input, input_format)
    {
        let filter = ResizeFilter::select_for(spec.filter, header_dimensions, header_dimensions);
        return Ok(TransformOutput {
            bytes: input.to_vec(),
            format: passthrough_format,
            source_dimensions: header_dimensions,
            output_dimensions: header_dimensions,
            filter,
            was_passthrough: true,
        });
    }

    let decoded = apply_flip(
        apply_rotation(
            decode_with_orientation(input, input_format, limits)?,
            spec.rotation,
        ),
        spec.filters.flip,
    );
    let source_dimensions = ImageDimensions::new(decoded.width(), decoded.height())
        .map_err(|err| EngineError::DecodeFailed(err.to_string()))?;

    let plan = plan_resize(source_dimensions, spec.dimensions, spec.fit, spec.gravity);
    let cropped = match plan.crop {
        Some(rect) => decoded.crop_imm(rect.x, rect.y, rect.width, rect.height),
        None => decoded,
    };
    let cropped_dimensions = ImageDimensions::new(cropped.width(), cropped.height())
        .map_err(|err| EngineError::DecodeFailed(err.to_string()))?;

    let filter = ResizeFilter::select_for(spec.filter, cropped_dimensions, plan.output);

    let resized = if plan.output == cropped_dimensions {
        cropped
    } else {
        resize_image(cropped, plan.output, filter.filter)?
    };

    let processed = apply_pixel_filters(resized, &spec.filters);
    let bytes = encode_image(processed, spec)?;

    Ok(TransformOutput {
        bytes,
        format: spec.format,
        source_dimensions,
        output_dimensions: plan.output,
        filter,
        was_passthrough: false,
    })
}

/// Inspects magic bytes and image headers without decoding pixel data.
///
/// Header dimensions are validated explicitly here so callers receive typed
/// limit errors (`SourceDimensionLimitExceeded`, `DecodedPixelLimitExceeded`)
/// before [`decode_with_orientation`] applies decoder-level [`Limits`] as
/// defence-in-depth.
fn inspect_header(
    input: &[u8],
    limits: &EngineLimits,
) -> Result<(ImageFormat, ImageDimensions), EngineError> {
    let reader = ImageReader::new(Cursor::new(input))
        .with_guessed_format()
        .map_err(|err| EngineError::DecodeFailed(err.to_string()))?;

    let format = reader.format().ok_or(EngineError::UnsupportedInputFormat)?;

    let (width, height) = reader
        .into_dimensions()
        .map_err(|err| EngineError::DecodeFailed(err.to_string()))?;

    let dimensions = ImageDimensions::new(width, height)
        .map_err(|err| EngineError::DecodeFailed(err.to_string()))?;

    if dimensions.width() > limits.max_source_dimension
        || dimensions.height() > limits.max_source_dimension
    {
        return Err(EngineError::SourceDimensionLimitExceeded {
            dimensions,
            max_dimension: limits.max_source_dimension,
        });
    }

    let pixels = dimensions.pixel_count();
    if pixels > limits.max_decoded_pixels {
        return Err(EngineError::DecodedPixelLimitExceeded {
            pixels,
            max_pixels: limits.max_decoded_pixels,
        });
    }

    Ok((format, dimensions))
}

/// Decodes a static image (or the first frame of an animated image) and applies EXIF orientation.
fn decode_with_orientation(
    input: &[u8],
    format: ImageFormat,
    limits: &EngineLimits,
) -> Result<DynamicImage, EngineError> {
    let mut reader = ImageReader::with_format(Cursor::new(input), format);
    reader.limits(build_decoder_limits(limits));

    let mut decoder = reader
        .into_decoder()
        .map_err(|err| EngineError::DecodeFailed(err.to_string()))?;

    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|err| EngineError::DecodeFailed(err.to_string()))?;

    image.apply_orientation(orientation);
    Ok(image)
}

/// Reports the matching [`OutputFormat`] when `input` is a multi-frame GIF or animated WebP.
fn detect_animated_format(input: &[u8], format: ImageFormat) -> Option<OutputFormat> {
    match format {
        ImageFormat::WebP => {
            let decoder = WebPDecoder::new(Cursor::new(input)).ok()?;
            decoder.has_animation().then_some(OutputFormat::Webp)
        }
        ImageFormat::Gif => {
            let decoder = GifDecoder::new(Cursor::new(input)).ok()?;
            let mut frames = decoder.into_frames();
            // Stop as soon as a second frame is observed to avoid decoding the entire animation.
            let has_first = frames.next().transpose().ok()?.is_some();
            let has_second = has_first && frames.next().transpose().ok()?.is_some();
            has_second.then_some(OutputFormat::Gif)
        }
        _ => None,
    }
}

/// Builds defence-in-depth decoder limits mirroring [`EngineLimits`].
fn build_decoder_limits(limits: &EngineLimits) -> Limits {
    let mut decoder_limits = Limits::default();
    decoder_limits.max_image_width = Some(limits.max_source_dimension);
    decoder_limits.max_image_height = Some(limits.max_source_dimension);
    // Allow 8 bytes/pixel headroom for 16-bit channels and decoder scratch buffers.
    decoder_limits.max_alloc = limits.max_decoded_pixels.checked_mul(8);
    decoder_limits
}

/// Resizes `image` to `target` dimensions using SIMD-accelerated `fast_image_resize`.
///
/// Opaque images are resampled in 3-channel `Rgb8` to minimise memory bandwidth;
/// images with an alpha channel are resampled in 4-channel `Rgba8` to preserve transparency.
fn resize_image(
    image: DynamicImage,
    target: ImageDimensions,
    filter: ResizeFilter,
) -> Result<DynamicImage, EngineError> {
    let options = ResizeOptions::new().resize_alg(map_resize_alg(filter));
    let mut resizer = Resizer::new();

    if image.color().has_alpha() {
        let src_rgba = image.into_rgba8();
        let src_image = Image::from_vec_u8(
            src_rgba.width(),
            src_rgba.height(),
            src_rgba.into_raw(),
            PixelType::U8x4,
        )
        .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;

        let mut dst_image = Image::new(target.width(), target.height(), PixelType::U8x4);
        resizer
            .resize(&src_image, &mut dst_image, &options)
            .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;

        let output_img = RgbaImage::from_raw(target.width(), target.height(), dst_image.into_vec())
            .ok_or_else(|| {
                EngineError::EncodeFailed("resized RGBA buffer length mismatch".to_owned())
            })?;
        Ok(DynamicImage::ImageRgba8(output_img))
    } else {
        let src_rgb = image.into_rgb8();
        let src_image = Image::from_vec_u8(
            src_rgb.width(),
            src_rgb.height(),
            src_rgb.into_raw(),
            PixelType::U8x3,
        )
        .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;

        let mut dst_image = Image::new(target.width(), target.height(), PixelType::U8x3);
        resizer
            .resize(&src_image, &mut dst_image, &options)
            .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;

        let output_img = RgbImage::from_raw(target.width(), target.height(), dst_image.into_vec())
            .ok_or_else(|| {
                EngineError::EncodeFailed("resized RGB buffer length mismatch".to_owned())
            })?;
        Ok(DynamicImage::ImageRgb8(output_img))
    }
}

/// Maps Murklet's domain [`ResizeFilter`] onto `fast_image_resize`'s SIMD resampling kernels.
const fn map_resize_alg(filter: ResizeFilter) -> ResizeAlg {
    match filter {
        ResizeFilter::Nearest => ResizeAlg::Nearest,
        ResizeFilter::Box => ResizeAlg::Convolution(FirFilterType::Box),
        ResizeFilter::Linear => ResizeAlg::Convolution(FirFilterType::Bilinear),
        ResizeFilter::Hermite => ResizeAlg::Convolution(FirFilterType::Hamming),
        ResizeFilter::CatmullRom => ResizeAlg::Convolution(FirFilterType::CatmullRom),
        ResizeFilter::BSpline => ResizeAlg::Convolution(FirFilterType::Mitchell),
        ResizeFilter::Gaussian => ResizeAlg::Convolution(FirFilterType::Gaussian),
        ResizeFilter::Lanczos => ResizeAlg::Convolution(FirFilterType::Lanczos3),
    }
}

fn apply_rotation(image: DynamicImage, rotation: Rotation) -> DynamicImage {
    match rotation {
        Rotation::Deg0 => image,
        Rotation::Deg90 => image.rotate90(),
        Rotation::Deg180 => image.rotate180(),
        Rotation::Deg270 => image.rotate270(),
    }
}

fn apply_flip(image: DynamicImage, flip: Flip) -> DynamicImage {
    match flip {
        Flip::None => image,
        Flip::Horizontal => image.fliph(),
        Flip::Vertical => image.flipv(),
        Flip::Both => image.fliph().flipv(),
    }
}

/// Applies post-resize pixel filters in Murklet's fixed canonical pipeline order.
fn apply_pixel_filters(mut image: DynamicImage, filters: &ImageFilters) -> DynamicImage {
    if filters.brightness.get() != 0 {
        image = image.brighten(i32::from(filters.brightness.get()));
    }
    if filters.contrast.get() != 0 {
        image = image.adjust_contrast(f32::from(filters.contrast.get()));
    }
    if filters.saturation.get() != 0 {
        image = apply_saturation(image, filters.saturation.get());
    }
    if filters.hue.degrees() != 0 {
        image = image.huerotate(i32::from(filters.hue.degrees()));
    }
    if !filters.gamma.is_identity() {
        image = apply_gamma(image, filters.gamma.as_f32());
    }
    if filters.grayscale {
        image = image.grayscale();
    }
    if filters.sepia.get() != 0 {
        image = apply_sepia(image, filters.sepia.get());
    }
    if let Some(tint) = filters.tint {
        image = apply_monochrome_tint(image, tint);
    }
    if let Some(duotone) = filters.duotone {
        image = apply_duotone(image, duotone);
    }
    if let Some(posterize) = filters.posterize {
        image = apply_posterize(image, posterize.levels());
    }
    if let Some(threshold) = filters.threshold {
        image = apply_threshold(image, threshold);
    }
    if filters.invert {
        image.invert();
    }
    if let Some(pixelate) = filters.pixelate {
        image = apply_pixelate(image, u32::from(pixelate.block_size()));
    }
    if let Some(blur) = filters.blur {
        image = image.fast_blur(blur.as_f32());
    }
    if let Some(sharpen) = filters.sharpen {
        image = image.unsharpen(sharpen.as_f32(), 1);
    }
    if filters.vignette.get() != 0 {
        image = apply_vignette(image, filters.vignette.get());
    }
    image
}

/// Applies a per-pixel RGB transform while preserving any alpha channel intact.
fn map_rgb_pixels<F>(image: DynamicImage, mut op: F) -> DynamicImage
where
    F: FnMut([u8; 3]) -> [u8; 3],
{
    if image.color().has_alpha() {
        let mut rgba = image.into_rgba8();
        for pixel in rgba.pixels_mut() {
            let [r, g, b] = op([pixel.0[0], pixel.0[1], pixel.0[2]]);
            pixel.0[0] = r;
            pixel.0[1] = g;
            pixel.0[2] = b;
        }
        DynamicImage::ImageRgba8(rgba)
    } else {
        let mut rgb = image.into_rgb8();
        for pixel in rgb.pixels_mut() {
            pixel.0 = op(pixel.0);
        }
        DynamicImage::ImageRgb8(rgb)
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn clamp_f32_to_u8(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

/// Adjusts saturation around Rec.709 luminance (`-100` = greyscale, `0` = identity, `+100` = 2×).
fn apply_saturation(image: DynamicImage, amount: i8) -> DynamicImage {
    let factor = 1.0 + f32::from(amount) / 100.0;
    map_rgb_pixels(image, |[r, g, b]| {
        let rf = f32::from(r);
        let gf = f32::from(g);
        let bf = f32::from(b);
        let luma = 0.2126 * rf + 0.7152 * gf + 0.0722 * bf;
        [
            clamp_f32_to_u8(luma + (rf - luma) * factor),
            clamp_f32_to_u8(luma + (gf - luma) * factor),
            clamp_f32_to_u8(luma + (bf - luma) * factor),
        ]
    })
}

/// Applies tonal gamma correction via a precomputed 256-byte lookup table (`> 1.0` brightens midtones).
fn apply_gamma(image: DynamicImage, gamma: f32) -> DynamicImage {
    let inv_gamma = 1.0 / gamma;
    let mut lut = [0_u8; 256];
    for (idx, slot) in lut.iter_mut().enumerate() {
        let norm = f32::from(u8::try_from(idx).unwrap_or(255)) / 255.0;
        *slot = clamp_f32_to_u8(norm.powf(inv_gamma) * 255.0);
    }
    map_rgb_pixels(image, |[r, g, b]| {
        [
            lut[usize::from(r)],
            lut[usize::from(g)],
            lut[usize::from(b)],
        ]
    })
}

/// Applies a vintage sepia matrix blended linearly with the original pixel by `amount` (`0..=100`).
fn apply_sepia(image: DynamicImage, amount: u8) -> DynamicImage {
    let strength = f32::from(amount) / 100.0;
    map_rgb_pixels(image, |[r, g, b]| {
        let rf = f32::from(r);
        let gf = f32::from(g);
        let bf = f32::from(b);
        let sr = (0.393 * rf + 0.769 * gf + 0.189 * bf).min(255.0);
        let sg = (0.349 * rf + 0.686 * gf + 0.168 * bf).min(255.0);
        let sb = (0.272 * rf + 0.534 * gf + 0.131 * bf).min(255.0);
        [
            clamp_f32_to_u8(rf + (sr - rf) * strength),
            clamp_f32_to_u8(gf + (sg - gf) * strength),
            clamp_f32_to_u8(bf + (sb - bf) * strength),
        ]
    })
}

/// Converts any image into a brand monochrome duotone (`black -> tint -> white`)
/// driven by Rec.709 luminance while keeping shadows crisp, highlights clean, and alpha intact.
fn apply_monochrome_tint(image: DynamicImage, tint: BackgroundColour) -> DynamicImage {
    map_rgb_pixels(image, |[r, g, b]| {
        // Exact 8-bit Rec.709 luminance (54 + 183 + 19 = 256).
        let luma = (54 * u32::from(r) + 183 * u32::from(g) + 19 * u32::from(b) + 128) >> 8;
        let ramp = |base: u8| -> u8 {
            let base_u32 = u32::from(base);
            let out = if luma <= 128 {
                (base_u32 * luma + 64) / 128
            } else {
                base_u32 + ((255 - base_u32) * (luma - 128) + 63) / 127
            };
            u8::try_from(out).unwrap_or(255)
        };
        [ramp(tint.r), ramp(tint.g), ramp(tint.b)]
    })
}

/// Maps Rec.709 luminance linearly between `duotone.dark` (`luma = 0`) and `duotone.light` (`luma = 255`).
fn apply_duotone(image: DynamicImage, duotone: Duotone) -> DynamicImage {
    map_rgb_pixels(image, |[r, g, b]| {
        let luma = (54 * u32::from(r) + 183 * u32::from(g) + 19 * u32::from(b) + 128) >> 8;
        let inv = 255 - luma;
        let lerp = |dark: u8, light: u8| -> u8 {
            let mixed = (u32::from(dark) * inv + u32::from(light) * luma + 127) / 255;
            u8::try_from(mixed).unwrap_or(255)
        };
        [
            lerp(duotone.dark.r, duotone.light.r),
            lerp(duotone.dark.g, duotone.light.g),
            lerp(duotone.dark.b, duotone.light.b),
        ]
    })
}

/// Quantises each RGB channel into `levels` discrete steps (`2..=16`) using a 256-byte LUT.
fn apply_posterize(image: DynamicImage, levels: u8) -> DynamicImage {
    let steps = f32::from(levels.saturating_sub(1).max(1));
    let mut lut = [0_u8; 256];
    for (idx, slot) in lut.iter_mut().enumerate() {
        let norm = f32::from(u8::try_from(idx).unwrap_or(255)) / 255.0;
        let quantised = (norm * steps).round() / steps;
        *slot = clamp_f32_to_u8(quantised * 255.0);
    }
    map_rgb_pixels(image, |[r, g, b]| {
        [
            lut[usize::from(r)],
            lut[usize::from(g)],
            lut[usize::from(b)],
        ]
    })
}

/// Binarises pixels into pure black or pure white based on Rec.709 luminance `>= threshold`.
fn apply_threshold(image: DynamicImage, threshold: u8) -> DynamicImage {
    let limit = u32::from(threshold);
    map_rgb_pixels(image, |[r, g, b]| {
        let luma = (54 * u32::from(r) + 183 * u32::from(g) + 19 * u32::from(b) + 128) >> 8;
        let v = if luma >= limit { 255 } else { 0 };
        [v, v, v]
    })
}

/// Replaces each `block_size × block_size` tile with its average RGB colour while preserving alpha.
fn apply_pixelate(image: DynamicImage, block_size: u32) -> DynamicImage {
    let mut rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();

    let mut by = 0;
    while by < height {
        let y_end = by.saturating_add(block_size).min(height);
        let mut bx = 0;
        while bx < width {
            let x_end = bx.saturating_add(block_size).min(width);
            let (mut sr, mut sg, mut sb, mut count) = (0_u64, 0_u64, 0_u64, 0_u64);
            for y in by..y_end {
                for x in bx..x_end {
                    let [r, g, b, _] = rgba.get_pixel(x, y).0;
                    sr += u64::from(r);
                    sg += u64::from(g);
                    sb += u64::from(b);
                    count += 1;
                }
            }
            if let (Some(ar_u64), Some(ag_u64), Some(ab_u64)) = (
                sr.checked_div(count),
                sg.checked_div(count),
                sb.checked_div(count),
            ) {
                let ar = u8::try_from(ar_u64).unwrap_or(255);
                let ag = u8::try_from(ag_u64).unwrap_or(255);
                let ab = u8::try_from(ab_u64).unwrap_or(255);
                for y in by..y_end {
                    for x in bx..x_end {
                        let px = rgba.get_pixel_mut(x, y);
                        px.0[0] = ar;
                        px.0[1] = ag;
                        px.0[2] = ab;
                    }
                }
            }
            bx = x_end;
        }
        by = y_end;
    }

    DynamicImage::ImageRgba8(rgba)
}

/// Applies smooth radial elliptical edge darkening (`strength` in `1..=100`) while preserving alpha.
#[allow(clippy::cast_precision_loss)]
fn apply_vignette(image: DynamicImage, strength: u8) -> DynamicImage {
    let mut rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();
    let cx = (width as f32) * 0.5;
    let cy = (height as f32) * 0.5;
    if cx <= 0.0 || cy <= 0.0 {
        return DynamicImage::ImageRgba8(rgba);
    }
    let intensity = f32::from(strength) / 100.0;

    for (x, y, px) in rgba.enumerate_pixels_mut() {
        let dx = ((x as f32 + 0.5) - cx) / cx;
        let dy = ((y as f32 + 0.5) - cy) / cy;
        // Normalised radial distance squared (0 at centre, ~2 at corners).
        let d2 = (dx * dx + dy * dy) * 0.5;
        let factor = (1.0 - intensity * d2).clamp(0.0, 1.0);
        px.0[0] = clamp_f32_to_u8(f32::from(px.0[0]) * factor);
        px.0[1] = clamp_f32_to_u8(f32::from(px.0[1]) * factor);
        px.0[2] = clamp_f32_to_u8(f32::from(px.0[2]) * factor);
    }

    DynamicImage::ImageRgba8(rgba)
}

/// Composites an alpha-bearing image over a solid RGB background colour using
/// exact integer blending (`(fg * a + bg * (255 - a) + 127) / 255`).
fn flatten_over_background(image: DynamicImage, bg: BackgroundColour) -> RgbImage {
    let rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();
    let mut rgb = RgbImage::new(width, height);

    for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
        let [sr, sg, sb, sa] = src.0;
        let alpha = u16::from(sa);
        let inv_alpha = 255 - alpha;

        let blend = |fg: u8, bg_ch: u8| -> u8 {
            let mixed = (u16::from(fg) * alpha + u16::from(bg_ch) * inv_alpha + 127) / 255;
            u8::try_from(mixed).unwrap_or(255)
        };

        dst.0 = [blend(sr, bg.r), blend(sg, bg.g), blend(sb, bg.b)];
    }

    rgb
}

/// Encodes a processed image into the format and quality requested in `spec`.
fn encode_image(image: DynamicImage, spec: &TransformSpec) -> Result<Vec<u8>, EngineError> {
    let mut output = Vec::new();

    match spec.format {
        OutputFormat::Jpeg => {
            let rgb = if image.color().has_alpha()
                && let Some(bg) = spec.background
            {
                flatten_over_background(image, bg)
            } else {
                image.into_rgb8()
            };
            let mut encoder = JpegEncoder::new_with_quality(&mut output, spec.quality.get());
            encoder
                .encode_image(&rgb)
                .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;
        }
        OutputFormat::Png => {
            image
                .write_to(&mut Cursor::new(&mut output), ImageFormat::Png)
                .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;
        }
        OutputFormat::Gif => {
            image
                .write_to(&mut Cursor::new(&mut output), ImageFormat::Gif)
                .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;
        }
        OutputFormat::Webp => {
            image
                .write_to(&mut Cursor::new(&mut output), ImageFormat::WebP)
                .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;
        }
        OutputFormat::Avif => {
            let rgba = image.into_rgba8();
            let encoder = AvifEncoder::new_with_speed_quality(
                &mut output,
                DEFAULT_AVIF_SPEED,
                spec.quality.get(),
            );
            encoder
                .write_image(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                    ExtendedColorType::Rgba8,
                )
                .map_err(|err| EngineError::EncodeFailed(err.to_string()))?;
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba, RgbaImage};

    use super::*;
    use crate::domain::{Axis, DEFAULT_MAX_DIMENSION, Dimension, Quality, TargetDimensions};

    fn make_png(width: u32, height: u32) -> Vec<u8> {
        let img = RgbaImage::from_fn(width, height, |x, y| {
            let r = u8::try_from((x * 7 + y * 3) % 256).unwrap();
            let g = u8::try_from((x * 5 + y * 11) % 256).unwrap();
            let b = u8::try_from((x * 13 + y * 17) % 256).unwrap();
            Rgba([r, g, b, 255])
        });
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn make_animated_gif(width: u32, height: u32, frame_count: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            for idx in 0..frame_count {
                let colour = idx.saturating_mul(60);
                let frame_img = RgbaImage::from_pixel(width, height, Rgba([colour, 100, 200, 255]));
                let frame = Frame::from_parts(frame_img, 0, 0, Delay::from_numer_denom_ms(100, 1));
                encoder.encode_frame(frame).unwrap();
            }
        }
        bytes
    }

    #[test]
    fn rejects_input_exceeding_max_bytes_before_decoding() {
        let payload = vec![0_u8; 64];
        let limits = EngineLimits {
            max_input_bytes: 32,
            ..EngineLimits::default()
        };

        let result = transform(&payload, &TransformSpec::default(), &limits);
        assert_eq!(
            result,
            Err(EngineError::InputTooLarge { size: 64, max: 32 })
        );
    }

    #[test]
    fn rejects_unrecognised_bytes() {
        let result = transform(
            b"not an image at all",
            &TransformSpec::default(),
            &EngineLimits::default(),
        );
        assert_eq!(result, Err(EngineError::UnsupportedInputFormat));
    }

    #[test]
    fn encodes_avif_with_valid_iso_bmff_container_signature() {
        let png = make_png(16, 16);
        let spec = TransformSpec {
            dimensions: TargetDimensions::new(
                Some(Dimension::new(Axis::Width, 8, DEFAULT_MAX_DIMENSION).unwrap()),
                None,
            ),
            format: OutputFormat::Avif,
            quality: Quality::new(70).unwrap(),
            filter: None,
            ..TransformSpec::default()
        };

        let out = transform(&png, &spec, &EngineLimits::default()).unwrap();
        assert_eq!(out.format, OutputFormat::Avif);
        assert_eq!(out.output_dimensions, ImageDimensions::new(8, 8).unwrap());
        assert!(out.bytes.len() > 16);
        // ISO BMFF box header has `ftyp` at bytes 4..8 and `avif` brand at 8..12.
        assert_eq!(&out.bytes[4..8], b"ftyp");
        assert_eq!(&out.bytes[8..12], b"avif");
    }

    #[test]
    fn enforces_source_dimension_and_decoded_pixel_limits_from_headers() {
        let png = make_png(100, 50);

        let dim_limits = EngineLimits {
            max_source_dimension: 80,
            ..EngineLimits::default()
        };
        assert_eq!(
            transform(&png, &TransformSpec::default(), &dim_limits),
            Err(EngineError::SourceDimensionLimitExceeded {
                dimensions: ImageDimensions::new(100, 50).unwrap(),
                max_dimension: 80,
            })
        );

        let pixel_limits = EngineLimits {
            max_decoded_pixels: 4_000,
            ..EngineLimits::default()
        };
        assert_eq!(
            transform(&png, &TransformSpec::default(), &pixel_limits),
            Err(EngineError::DecodedPixelLimitExceeded {
                pixels: 5_000,
                max_pixels: 4_000,
            })
        );
    }

    #[test]
    fn resizes_proportionally_and_encodes_jpeg_png_webp_gif() {
        let png = make_png(80, 40);

        for format in [
            OutputFormat::Jpeg,
            OutputFormat::Png,
            OutputFormat::Webp,
            OutputFormat::Gif,
        ] {
            let spec = TransformSpec {
                dimensions: TargetDimensions::new(
                    Some(Dimension::new(Axis::Width, 40, DEFAULT_MAX_DIMENSION).unwrap()),
                    None,
                ),
                format,
                quality: Quality::new(80).unwrap(),
                filter: None,
                ..TransformSpec::default()
            };

            let out = transform(&png, &spec, &EngineLimits::default()).unwrap();
            assert_eq!(out.format, format);
            assert_eq!(out.source_dimensions, ImageDimensions::new(80, 40).unwrap());
            assert_eq!(out.output_dimensions, ImageDimensions::new(40, 20).unwrap());
            assert_eq!(out.filter.filter, ResizeFilter::Lanczos);
            assert!(out.filter.is_auto);
            assert!(!out.was_passthrough);

            let decoded = ImageReader::new(Cursor::new(&out.bytes))
                .with_guessed_format()
                .unwrap()
                .decode()
                .unwrap();
            assert_eq!((decoded.width(), decoded.height()), (40, 20));
        }
    }

    #[test]
    fn jpeg_quality_controls_encoded_size() {
        let png = make_png(128, 128);

        let low_q = transform(
            &png,
            &TransformSpec {
                format: OutputFormat::Jpeg,
                quality: Quality::new(20).unwrap(),
                ..TransformSpec::default()
            },
            &EngineLimits::default(),
        )
        .unwrap();

        let high_q = transform(
            &png,
            &TransformSpec {
                format: OutputFormat::Jpeg,
                quality: Quality::new(95).unwrap(),
                ..TransformSpec::default()
            },
            &EngineLimits::default(),
        )
        .unwrap();

        assert!(high_q.bytes.len() > low_q.bytes.len());
    }

    #[test]
    fn animated_gif_follows_first_frame_and_passthrough_policies() {
        let animated_gif = make_animated_gif(32, 16, 3);
        let spec = TransformSpec {
            dimensions: TargetDimensions::new(
                Some(Dimension::new(Axis::Width, 16, DEFAULT_MAX_DIMENSION).unwrap()),
                None,
            ),
            format: OutputFormat::Png,
            quality: Quality::DEFAULT,
            filter: None,
            ..TransformSpec::default()
        };

        // Default policy: transforms the first frame to the requested format and size.
        let first_frame = transform(&animated_gif, &spec, &EngineLimits::default()).unwrap();
        assert!(!first_frame.was_passthrough);
        assert_eq!(first_frame.format, OutputFormat::Png);
        assert_eq!(
            first_frame.output_dimensions,
            ImageDimensions::new(16, 8).unwrap()
        );

        // Passthrough policy: preserves original animated bytes and GIF format unchanged.
        let passthrough_limits = EngineLimits {
            animation_policy: AnimationPolicy::Passthrough,
            ..EngineLimits::default()
        };
        let passthrough = transform(&animated_gif, &spec, &passthrough_limits).unwrap();
        assert!(passthrough.was_passthrough);
        assert_eq!(passthrough.format, OutputFormat::Gif);
        assert_eq!(passthrough.bytes, animated_gif);
        assert_eq!(
            passthrough.output_dimensions,
            ImageDimensions::new(32, 16).unwrap()
        );
    }

    #[test]
    fn cover_crop_rotation_and_background_flatten_work_end_to_end() {
        use crate::domain::{FitMode, Gravity};

        // 80x40 rotated 90 degrees becomes 40x80, then cover-cropped to 20x20.
        let png = make_png(80, 40);
        let spec = TransformSpec {
            dimensions: TargetDimensions::new(
                Some(Dimension::new(Axis::Width, 20, DEFAULT_MAX_DIMENSION).unwrap()),
                Some(Dimension::new(Axis::Height, 20, DEFAULT_MAX_DIMENSION).unwrap()),
            ),
            format: OutputFormat::Png,
            fit: FitMode::Cover,
            gravity: Gravity::North,
            rotation: Rotation::Deg90,
            ..TransformSpec::default()
        };
        let out = transform(&png, &spec, &EngineLimits::default()).unwrap();
        assert_eq!(out.source_dimensions, ImageDimensions::new(40, 80).unwrap());
        assert_eq!(out.output_dimensions, ImageDimensions::new(20, 20).unwrap());

        // Transparent 16x16 PNG flattened onto pure white JPEG should decode near white.
        let transparent = RgbaImage::from_pixel(16, 16, Rgba([0, 0, 0, 0]));
        let mut transparent_png = Vec::new();
        DynamicImage::ImageRgba8(transparent)
            .write_to(&mut Cursor::new(&mut transparent_png), ImageFormat::Png)
            .unwrap();

        let flat_spec = TransformSpec {
            format: OutputFormat::Jpeg,
            quality: Quality::new(95).unwrap(),
            background: Some("ffffff".parse().unwrap()),
            ..TransformSpec::default()
        };
        let flat = transform(&transparent_png, &flat_spec, &EngineLimits::default()).unwrap();
        let decoded = ImageReader::new(Cursor::new(&flat.bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .into_rgb8();
        let pixel = decoded.get_pixel(8, 8).0;
        assert!(pixel[0] >= 250 && pixel[1] >= 250 && pixel[2] >= 250);
    }

    #[test]
    fn negative_grayscale_and_blur_filters_transform_pixels_deterministically() {
        let solid = RgbaImage::from_pixel(16, 16, Rgba([10, 20, 30, 255]));
        let mut solid_png = Vec::new();
        DynamicImage::ImageRgba8(solid)
            .write_to(&mut Cursor::new(&mut solid_png), ImageFormat::Png)
            .unwrap();

        // Negative filter inverts RGB channels: (10, 20, 30) -> (245, 235, 225).
        let neg_spec = TransformSpec {
            format: OutputFormat::Png,
            filters: ImageFilters {
                invert: true,
                ..ImageFilters::default()
            },
            ..TransformSpec::default()
        };
        let neg = transform(&solid_png, &neg_spec, &EngineLimits::default()).unwrap();
        let neg_img = ImageReader::new(Cursor::new(&neg.bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .into_rgb8();
        assert_eq!(neg_img.get_pixel(4, 4).0, [245, 235, 225]);

        // Grayscale + blur pipeline runs cleanly and yields equal R=G=B channels.
        let gs_spec = TransformSpec {
            format: OutputFormat::Png,
            filters: ImageFilters {
                grayscale: true,
                blur: Some("1.5".parse().unwrap()),
                ..ImageFilters::default()
            },
            ..TransformSpec::default()
        };
        let gs = transform(&solid_png, &gs_spec, &EngineLimits::default()).unwrap();
        let gs_img = ImageReader::new(Cursor::new(&gs.bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .into_rgb8();
        let px = gs_img.get_pixel(8, 8).0;
        assert_eq!(px[0], px[1]);
        assert_eq!(px[1], px[2]);
    }

    #[test]
    fn monochrome_tint_maps_shadows_midtones_highlights_and_preserves_alpha() {
        // 3x1 RGBA strip: pure black, exact mid-grey, pure white — all with alpha=180.
        let mut strip = RgbaImage::new(3, 1);
        strip.put_pixel(0, 0, Rgba([0, 0, 0, 180]));
        strip.put_pixel(1, 0, Rgba([128, 128, 128, 180]));
        strip.put_pixel(2, 0, Rgba([255, 255, 255, 180]));
        let mut strip_png = Vec::new();
        DynamicImage::ImageRgba8(strip)
            .write_to(&mut Cursor::new(&mut strip_png), ImageFormat::Png)
            .unwrap();

        let brand: BackgroundColour = "1a73e8".parse().unwrap();
        let spec = TransformSpec {
            format: OutputFormat::Png,
            filters: ImageFilters {
                tint: Some(brand),
                ..ImageFilters::default()
            },
            ..TransformSpec::default()
        };
        let out = transform(&strip_png, &spec, &EngineLimits::default()).unwrap();
        let decoded = ImageReader::new(Cursor::new(&out.bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .into_rgba8();

        // Shadows stay black, midtones hit exact brand colour, highlights stay white; alpha intact.
        assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 0, 180]);
        assert_eq!(decoded.get_pixel(1, 0).0, [0x1a, 0x73, 0xe8, 180]);
        assert_eq!(decoded.get_pixel(2, 0).0, [255, 255, 255, 180]);
    }

    #[test]
    fn saturation_gamma_hue_and_sepia_transform_pixels_cleanly() {
        use crate::domain::{Gamma, Hue, Saturation, Sepia};

        let solid = RgbaImage::from_pixel(8, 8, Rgba([200, 100, 50, 255]));
        let mut solid_png = Vec::new();
        DynamicImage::ImageRgba8(solid)
            .write_to(&mut Cursor::new(&mut solid_png), ImageFormat::Png)
            .unwrap();

        // Saturation -100 produces equal R=G=B channels.
        let desat_spec = TransformSpec {
            format: OutputFormat::Png,
            filters: ImageFilters {
                saturation: Saturation::new(-100).unwrap(),
                ..ImageFilters::default()
            },
            ..TransformSpec::default()
        };
        let desat = transform(&solid_png, &desat_spec, &EngineLimits::default()).unwrap();
        let desat_img = ImageReader::new(Cursor::new(&desat.bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .into_rgb8();
        let px = desat_img.get_pixel(4, 4).0;
        assert_eq!(px[0], px[1]);
        assert_eq!(px[1], px[2]);

        // Gamma 2.0 brightens midtones; hue + sepia execute cleanly without panics.
        let combo_spec = TransformSpec {
            format: OutputFormat::Png,
            filters: ImageFilters {
                gamma: "2.0".parse::<Gamma>().unwrap(),
                hue: Hue::new(90).unwrap(),
                sepia: Sepia::new(75).unwrap(),
                ..ImageFilters::default()
            },
            ..TransformSpec::default()
        };
        let combo = transform(&solid_png, &combo_spec, &EngineLimits::default()).unwrap();
        assert!(!combo.bytes.is_empty());
    }

    #[test]
    fn duotone_threshold_posterize_pixelate_and_vignette_work_deterministically() {
        use crate::domain::{Pixelate, Posterize, Vignette};

        // 4x1 strip: black, dark-grey, light-grey, white (alpha=200).
        let mut strip = RgbaImage::new(4, 1);
        strip.put_pixel(0, 0, Rgba([0, 0, 0, 200]));
        strip.put_pixel(1, 0, Rgba([80, 80, 80, 200]));
        strip.put_pixel(2, 0, Rgba([180, 180, 180, 200]));
        strip.put_pixel(3, 0, Rgba([255, 255, 255, 200]));
        let mut strip_png = Vec::new();
        DynamicImage::ImageRgba8(strip)
            .write_to(&mut Cursor::new(&mut strip_png), ImageFormat::Png)
            .unwrap();

        // Duotone maps luma=0 -> dark (#102030) and luma=255 -> light (#f0e0d0), keeping alpha.
        let duo_spec = TransformSpec {
            format: OutputFormat::Png,
            filters: ImageFilters {
                duotone: Some("102030:f0e0d0".parse().unwrap()),
                ..ImageFilters::default()
            },
            ..TransformSpec::default()
        };
        let duo = transform(&strip_png, &duo_spec, &EngineLimits::default()).unwrap();
        let duo_img = ImageReader::new(Cursor::new(&duo.bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .into_rgba8();
        assert_eq!(duo_img.get_pixel(0, 0).0, [0x10, 0x20, 0x30, 200]);
        assert_eq!(duo_img.get_pixel(3, 0).0, [0xf0, 0xe0, 0xd0, 200]);

        // Threshold at 128 turns 0/80 -> 0 and 180/255 -> 255.
        let thr_spec = TransformSpec {
            format: OutputFormat::Png,
            filters: ImageFilters {
                threshold: Some(128),
                ..ImageFilters::default()
            },
            ..TransformSpec::default()
        };
        let thr = transform(&strip_png, &thr_spec, &EngineLimits::default()).unwrap();
        let thr_img = ImageReader::new(Cursor::new(&thr.bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .into_rgba8();
        assert_eq!(thr_img.get_pixel(1, 0).0, [0, 0, 0, 200]);
        assert_eq!(thr_img.get_pixel(2, 0).0, [255, 255, 255, 200]);

        // Posterize + pixelate + vignette execute cleanly on a 16x16 image.
        let img = make_png(16, 16);
        let creative_spec = TransformSpec {
            format: OutputFormat::Png,
            filters: ImageFilters {
                posterize: Some(Posterize::new(4).unwrap()),
                pixelate: Some(Pixelate::new(4).unwrap()),
                vignette: Vignette::new(60).unwrap(),
                ..ImageFilters::default()
            },
            ..TransformSpec::default()
        };
        let out = transform(&img, &creative_spec, &EngineLimits::default()).unwrap();
        assert!(!out.bytes.is_empty());
    }
}

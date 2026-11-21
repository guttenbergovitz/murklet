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

use crate::domain::{ImageDimensions, OutputFormat, ResizeFilter, SelectedFilter, TransformSpec};
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

    let decoded = decode_with_orientation(input, input_format, limits)?;
    let source_dimensions = ImageDimensions::new(decoded.width(), decoded.height())
        .map_err(|err| EngineError::DecodeFailed(err.to_string()))?;

    let output_dimensions = spec.dimensions.resolve(source_dimensions);
    let filter = ResizeFilter::select_for(spec.filter, source_dimensions, output_dimensions);

    let processed = if output_dimensions == source_dimensions {
        decoded
    } else {
        resize_image(decoded, output_dimensions, filter.filter)?
    };

    let bytes = encode_image(processed, spec)?;

    Ok(TransformOutput {
        bytes,
        format: spec.format,
        source_dimensions,
        output_dimensions,
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

/// Encodes a processed image into the format and quality requested in `spec`.
fn encode_image(image: DynamicImage, spec: &TransformSpec) -> Result<Vec<u8>, EngineError> {
    let mut output = Vec::new();

    match spec.format {
        OutputFormat::Jpeg => {
            let rgb = image.into_rgb8();
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
}

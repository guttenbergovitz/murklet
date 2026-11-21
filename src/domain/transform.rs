//! Value types and canonical specification for image transformations.
//!
//! Each type enforces its own invariants on construction so downstream layers
//! (policy, engine, cache, HTTP) never need to re-validate raw primitives.

use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

use crate::domain::error::{Axis, DomainError};
use crate::domain::filters::ImageFilters;
use crate::domain::geometry::{BackgroundColour, FitMode, Gravity, Rotation};
use crate::domain::watermark::WatermarkSpec;

/// Default maximum width or height accepted by baseline transformation requests.
pub const DEFAULT_MAX_DIMENSION: u32 = 8192;

/// Current version prefix embedded in canonical transformation identities.
///
/// Bumping this version invalidates previously cached variants whenever
/// transformation or encoding semantics change materially.
pub const CANONICAL_SPEC_VERSION: u8 = 1;

/// A validated, non-zero target dimension bounded by a configured maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Dimension(NonZeroU32);

impl Dimension {
    /// Constructs a validated dimension within `1..=max`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::DimensionOutOfRange`] if `value` is `0` or exceeds `max`.
    pub fn new(axis: Axis, value: u32, max: u32) -> Result<Self, DomainError> {
        if value == 0 || value > max {
            return Err(DomainError::DimensionOutOfRange { axis, value, max });
        }

        NonZeroU32::new(value)
            .map(Self)
            .ok_or(DomainError::DimensionOutOfRange { axis, value, max })
    }

    /// Returns the validated pixel count as a `u32`.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// Returns the underlying [`NonZeroU32`].
    #[must_use]
    pub const fn as_non_zero(self) -> NonZeroU32 {
        self.0
    }
}

/// Concrete, non-zero pixel dimensions of a decoded or resolved image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageDimensions {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl ImageDimensions {
    /// Constructs concrete image dimensions from raw pixel counts.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::ZeroSourceDimension`] if either `width` or `height` is zero.
    pub fn new(width: u32, height: u32) -> Result<Self, DomainError> {
        let width =
            NonZeroU32::new(width).ok_or(DomainError::ZeroSourceDimension { axis: Axis::Width })?;
        let height = NonZeroU32::new(height)
            .ok_or(DomainError::ZeroSourceDimension { axis: Axis::Height })?;

        Ok(Self { width, height })
    }

    /// Constructs concrete image dimensions from already-validated [`NonZeroU32`] axes.
    #[must_use]
    pub const fn from_non_zero(width: NonZeroU32, height: NonZeroU32) -> Self {
        Self { width, height }
    }

    /// Returns the image width in pixels.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width.get()
    }

    /// Returns the image height in pixels.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height.get()
    }

    /// Returns the total pixel count (`width * height`) without `u32` overflow.
    #[must_use]
    pub const fn pixel_count(self) -> u64 {
        (self.width.get() as u64) * (self.height.get() as u64)
    }
}

/// Optional target width and height requested by a client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct TargetDimensions {
    width: Option<Dimension>,
    height: Option<Dimension>,
}

impl TargetDimensions {
    /// Creates a target dimension specification from optional validated bounds.
    #[must_use]
    pub const fn new(width: Option<Dimension>, height: Option<Dimension>) -> Self {
        Self { width, height }
    }

    /// Returns the requested target width, if set.
    #[must_use]
    pub const fn width(self) -> Option<Dimension> {
        self.width
    }

    /// Returns the requested target height, if set.
    #[must_use]
    pub const fn height(self) -> Option<Dimension> {
        self.height
    }

    /// Resolves concrete output dimensions against a source image's dimensions.
    ///
    /// - Neither set: preserves `source` unchanged.
    /// - Only one axis set: scales the other axis proportionally using exact
    ///   integer arithmetic, floored with a minimum of `1` pixel.
    /// - Both set: returns the exact requested dimensions.
    #[must_use]
    pub fn resolve(self, source: ImageDimensions) -> ImageDimensions {
        match (self.width, self.height) {
            (None, None) => source,
            (Some(target_w), Some(target_h)) => ImageDimensions {
                width: target_w.as_non_zero(),
                height: target_h.as_non_zero(),
            },
            (Some(target_w), None) => {
                let scaled_h =
                    scale_proportionally(source.height(), target_w.get(), source.width());
                ImageDimensions {
                    width: target_w.as_non_zero(),
                    height: scaled_h,
                }
            }
            (None, Some(target_h)) => {
                let scaled_w =
                    scale_proportionally(source.width(), target_h.get(), source.height());
                ImageDimensions {
                    width: scaled_w,
                    height: target_h.as_non_zero(),
                }
            }
        }
    }
}

/// Computes `max(1, floor(numerator_a * numerator_b / denominator))` in `u64`
/// and saturates at `u32::MAX` so extreme aspect ratios never overflow.
fn scale_proportionally(numerator_a: u32, numerator_b: u32, denominator: u32) -> NonZeroU32 {
    let scaled = (u64::from(numerator_a) * u64::from(numerator_b)) / u64::from(denominator);
    let clamped = u32::try_from(scaled).unwrap_or(u32::MAX).max(1);
    NonZeroU32::new(clamped).unwrap_or(NonZeroU32::MIN)
}

/// Lossy output quality in the inclusive range `1..=100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Quality(u8);

impl Quality {
    /// Baseline default encoding quality (`75`).
    pub const DEFAULT: Self = Self(75);

    /// Constructs a validated quality value within `1..=100`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::QualityOutOfRange`] if `value` is `0` or greater than `100`.
    pub const fn new(value: u8) -> Result<Self, DomainError> {
        if value == 0 || value > 100 {
            return Err(DomainError::QualityOutOfRange { value });
        }
        Ok(Self(value))
    }

    /// Returns the numeric quality value (`1..=100`).
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for Quality {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Supported encoded output formats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    /// JPEG (`image/jpeg`). Default format when none is specified.
    #[default]
    Jpeg,
    /// PNG (`image/png`). Lossless.
    Png,
    /// GIF (`image/gif`). Lossless palette-based format.
    Gif,
    /// WebP (`image/webp`).
    Webp,
    /// AVIF (`image/avif`).
    Avif,
}

impl OutputFormat {
    /// Returns the canonical lowercase identifier for this format.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Avif => "avif",
        }
    }

    /// Returns the standard HTTP `Content-Type` MIME type for this format.
    #[must_use]
    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Avif => "image/avif",
        }
    }

    /// Returns the conventional file extension (without leading dot).
    #[must_use]
    pub const fn file_extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Avif => "avif",
        }
    }

    /// Reports whether [`Quality`] affects the encoded output of this format.
    ///
    /// Lossless formats ignore quality during canonical cache-key generation so
    /// requests differing only in `q` share a single cached variant.
    #[must_use]
    pub const fn is_lossy(self) -> bool {
        matches!(self, Self::Jpeg | Self::Webp | Self::Avif)
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OutputFormat {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Ok(Self::Jpeg),
            "png" => Ok(Self::Png),
            "gif" => Ok(Self::Gif),
            "webp" => Ok(Self::Webp),
            "avif" => Ok(Self::Avif),
            _ => Err(DomainError::UnsupportedFormat(raw.to_owned())),
        }
    }
}

/// Resampling kernel used when resizing an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResizeFilter {
    /// Nearest-neighbour sampling.
    Nearest,
    /// Box averaging filter.
    Box,
    /// Bilinear interpolation.
    Linear,
    /// Hermite cubic filter.
    Hermite,
    /// Catmull-Rom cubic filter (default for upscaling).
    CatmullRom,
    /// Cubic B-spline filter.
    BSpline,
    /// Gaussian filter.
    Gaussian,
    /// Lanczos windowed sinc filter (default for downscaling).
    Lanczos,
}

impl ResizeFilter {
    /// Returns the canonical lowercase filter name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nearest => "nearest",
            Self::Box => "box",
            Self::Linear => "linear",
            Self::Hermite => "hermite",
            Self::CatmullRom => "catmullrom",
            Self::BSpline => "bspline",
            Self::Gaussian => "gaussian",
            Self::Lanczos => "lanczos",
        }
    }

    /// Resolves the effective resampling filter for a source-to-target resize.
    ///
    /// When no filter is explicitly requested, selects [`Self::CatmullRom`] for
    /// upscaling (`target_area > source_area`) and [`Self::Lanczos`] otherwise.
    #[must_use]
    pub const fn select_for(
        requested: Option<Self>,
        source: ImageDimensions,
        target: ImageDimensions,
    ) -> SelectedFilter {
        if let Some(filter) = requested {
            return SelectedFilter {
                filter,
                is_auto: false,
            };
        }

        let filter = if target.pixel_count() > source.pixel_count() {
            Self::CatmullRom
        } else {
            Self::Lanczos
        };

        SelectedFilter {
            filter,
            is_auto: true,
        }
    }
}

impl fmt::Display for ResizeFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ResizeFilter {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.to_ascii_lowercase().as_str() {
            "nearest" => Ok(Self::Nearest),
            "box" => Ok(Self::Box),
            "linear" => Ok(Self::Linear),
            "hermite" => Ok(Self::Hermite),
            "catmullrom" => Ok(Self::CatmullRom),
            "bspline" => Ok(Self::BSpline),
            "gaussian" => Ok(Self::Gaussian),
            "lanczos" => Ok(Self::Lanczos),
            _ => Err(DomainError::UnsupportedFilter(raw.to_owned())),
        }
    }
}

/// Concrete resampling filter paired with whether it was chosen automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SelectedFilter {
    /// The resampling kernel to apply.
    pub filter: ResizeFilter,
    /// `true` when Murklet chose the filter automatically because the caller omitted `filter`.
    pub is_auto: bool,
}

impl fmt::Display for SelectedFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_auto {
            write!(f, "{} (auto-selected)", self.filter)
        } else {
            write!(f, "{}", self.filter)
        }
    }
}

/// Canonical, transport-agnostic specification of an image transformation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TransformSpec {
    /// Optional target width and height.
    pub dimensions: TargetDimensions,
    /// Encoded output format.
    pub format: OutputFormat,
    /// Encoding quality (meaningful only for lossy formats).
    pub quality: Quality,
    /// Explicit resampling filter, or `None` for automatic selection.
    pub filter: Option<ResizeFilter>,
    /// Fit/crop strategy when target dimensions are provided.
    pub fit: FitMode,
    /// Crop anchor or focal point (meaningful when `fit == FitMode::Cover`).
    pub gravity: Gravity,
    /// Clockwise lossless rotation (`0`, `90`, `180`, `270`).
    pub rotation: Rotation,
    /// Optional RGB background colour used to flatten alpha on opaque outputs (such as JPEG).
    pub background: Option<BackgroundColour>,
    /// Post-resize pixel filters and mirror flip.
    pub filters: ImageFilters,
    /// Optional policy-controlled watermark overlay.
    pub watermark: Option<WatermarkSpec>,
}

impl TransformSpec {
    /// Produces a deterministic, versioned canonical string describing this transformation.
    ///
    /// Equivalent requests (for example `f=jpg` and `f=jpeg`, or lossless PNGs with
    /// differing `q` values) always yield the exact same canonical representation,
    /// making it suitable as input to cache-key hashing.
    #[must_use]
    pub fn canonical_representation(&self) -> String {
        let mut out = String::with_capacity(64);
        out.push('v');
        out.push_str(&CANONICAL_SPEC_VERSION.to_string());
        out.push_str("|f=");
        out.push_str(self.format.as_str());
        out.push_str("|q=");
        if self.format.is_lossy() {
            out.push_str(&self.quality.get().to_string());
        } else {
            out.push('-');
        }
        out.push_str("|w=");
        match self.dimensions.width() {
            Some(w) => out.push_str(&w.get().to_string()),
            None => out.push('-'),
        }
        out.push_str("|h=");
        match self.dimensions.height() {
            Some(h) => out.push_str(&h.get().to_string()),
            None => out.push('-'),
        }
        out.push_str("|flt=");
        match self.filter {
            Some(filter) => out.push_str(filter.as_str()),
            None => out.push_str("auto"),
        }
        if self.fit != FitMode::Fill {
            out.push_str("|fit=");
            out.push_str(self.fit.as_str());
        }
        if self.fit == FitMode::Cover && self.gravity != Gravity::Center {
            out.push_str("|g=");
            out.push_str(&self.gravity.canonical_str());
        }
        if self.rotation != Rotation::Deg0 {
            out.push_str("|rot=");
            out.push_str(&self.rotation.degrees().to_string());
        }
        if let Some(bg) = self.background {
            out.push_str("|bg=");
            out.push_str(&bg.to_hex());
        }
        self.filters.append_canonical(&mut out);
        if let Some(ref wm) = self.watermark {
            wm.append_canonical(&mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_rejects_zero_and_values_above_max() {
        assert_eq!(
            Dimension::new(Axis::Width, 0, DEFAULT_MAX_DIMENSION),
            Err(DomainError::DimensionOutOfRange {
                axis: Axis::Width,
                value: 0,
                max: DEFAULT_MAX_DIMENSION,
            })
        );

        assert_eq!(
            Dimension::new(
                Axis::Height,
                DEFAULT_MAX_DIMENSION + 1,
                DEFAULT_MAX_DIMENSION
            ),
            Err(DomainError::DimensionOutOfRange {
                axis: Axis::Height,
                value: DEFAULT_MAX_DIMENSION + 1,
                max: DEFAULT_MAX_DIMENSION,
            })
        );

        let min = Dimension::new(Axis::Width, 1, DEFAULT_MAX_DIMENSION).unwrap();
        let max =
            Dimension::new(Axis::Height, DEFAULT_MAX_DIMENSION, DEFAULT_MAX_DIMENSION).unwrap();
        assert_eq!(min.get(), 1);
        assert_eq!(max.get(), DEFAULT_MAX_DIMENSION);
    }

    #[test]
    fn image_dimensions_reject_zero_axes_and_avoid_pixel_count_overflow() {
        assert_eq!(
            ImageDimensions::new(0, 100),
            Err(DomainError::ZeroSourceDimension { axis: Axis::Width })
        );
        assert_eq!(
            ImageDimensions::new(100, 0),
            Err(DomainError::ZeroSourceDimension { axis: Axis::Height })
        );

        let large = ImageDimensions::new(100_000, 100_000).unwrap();
        assert_eq!(large.pixel_count(), 10_000_000_000);
    }

    #[test]
    fn target_dimensions_preserve_aspect_ratio_and_support_exact_sizing() {
        let source = ImageDimensions::new(1920, 1080).unwrap();

        // Neither axis set preserves original dimensions.
        assert_eq!(TargetDimensions::default().resolve(source), source);

        // Width only scales height proportionally.
        let w_only = TargetDimensions::new(
            Some(Dimension::new(Axis::Width, 960, DEFAULT_MAX_DIMENSION).unwrap()),
            None,
        );
        assert_eq!(
            w_only.resolve(source),
            ImageDimensions::new(960, 540).unwrap()
        );

        // Height only scales width proportionally.
        let h_only = TargetDimensions::new(
            None,
            Some(Dimension::new(Axis::Height, 270, DEFAULT_MAX_DIMENSION).unwrap()),
        );
        assert_eq!(
            h_only.resolve(source),
            ImageDimensions::new(480, 270).unwrap()
        );

        // Both set uses exact requested dimensions.
        let both = TargetDimensions::new(
            Some(Dimension::new(Axis::Width, 300, DEFAULT_MAX_DIMENSION).unwrap()),
            Some(Dimension::new(Axis::Height, 300, DEFAULT_MAX_DIMENSION).unwrap()),
        );
        assert_eq!(
            both.resolve(source),
            ImageDimensions::new(300, 300).unwrap()
        );
    }

    #[test]
    fn target_dimensions_clamp_extreme_aspect_ratios_to_at_least_one_pixel() {
        let panoramic = ImageDimensions::new(10_000, 10).unwrap();
        let tiny_width = TargetDimensions::new(
            Some(Dimension::new(Axis::Width, 5, DEFAULT_MAX_DIMENSION).unwrap()),
            None,
        );

        assert_eq!(
            tiny_width.resolve(panoramic),
            ImageDimensions::new(5, 1).unwrap()
        );
    }

    #[test]
    fn quality_enforces_inclusive_bounds_and_default() {
        assert_eq!(Quality::default().get(), 75);
        assert_eq!(
            Quality::new(0),
            Err(DomainError::QualityOutOfRange { value: 0 })
        );
        assert_eq!(
            Quality::new(101),
            Err(DomainError::QualityOutOfRange { value: 101 })
        );
        assert_eq!(Quality::new(1).unwrap().get(), 1);
        assert_eq!(Quality::new(100).unwrap().get(), 100);
    }

    #[test]
    fn output_format_normalises_aliases_and_rejects_unknown_values() {
        assert_eq!("jpg".parse::<OutputFormat>().unwrap(), OutputFormat::Jpeg);
        assert_eq!("JPEG".parse::<OutputFormat>().unwrap(), OutputFormat::Jpeg);
        assert_eq!("WebP".parse::<OutputFormat>().unwrap(), OutputFormat::Webp);
        assert_eq!("png".parse::<OutputFormat>().unwrap(), OutputFormat::Png);
        assert_eq!("gif".parse::<OutputFormat>().unwrap(), OutputFormat::Gif);
        assert_eq!("avif".parse::<OutputFormat>().unwrap(), OutputFormat::Avif);

        assert_eq!(
            "bmp".parse::<OutputFormat>(),
            Err(DomainError::UnsupportedFormat("bmp".to_owned()))
        );

        assert!(OutputFormat::Jpeg.is_lossy());
        assert!(OutputFormat::Webp.is_lossy());
        assert!(OutputFormat::Avif.is_lossy());
        assert!(!OutputFormat::Png.is_lossy());
        assert!(!OutputFormat::Gif.is_lossy());
    }

    #[test]
    fn resize_filter_auto_selects_based_on_area_change() {
        let source = ImageDimensions::new(800, 600).unwrap();
        let smaller = ImageDimensions::new(400, 300).unwrap();
        let larger = ImageDimensions::new(1600, 1200).unwrap();

        let down = ResizeFilter::select_for(None, source, smaller);
        assert_eq!(down.filter, ResizeFilter::Lanczos);
        assert!(down.is_auto);
        assert_eq!(down.to_string(), "lanczos (auto-selected)");

        let same = ResizeFilter::select_for(None, source, source);
        assert_eq!(same.filter, ResizeFilter::Lanczos);
        assert!(same.is_auto);

        let up = ResizeFilter::select_for(None, source, larger);
        assert_eq!(up.filter, ResizeFilter::CatmullRom);
        assert!(up.is_auto);

        let explicit = ResizeFilter::select_for(Some(ResizeFilter::Nearest), source, larger);
        assert_eq!(explicit.filter, ResizeFilter::Nearest);
        assert!(!explicit.is_auto);
        assert_eq!(explicit.to_string(), "nearest");
    }

    #[test]
    fn resize_filter_parses_known_values_and_rejects_unknown() {
        assert_eq!(
            "CatmullRom".parse::<ResizeFilter>().unwrap(),
            ResizeFilter::CatmullRom
        );
        assert_eq!(
            "lanczos".parse::<ResizeFilter>().unwrap(),
            ResizeFilter::Lanczos
        );
        assert_eq!(
            "bicubic".parse::<ResizeFilter>(),
            Err(DomainError::UnsupportedFilter("bicubic".to_owned()))
        );
    }

    #[test]
    fn canonical_representation_normalises_lossless_quality_and_format_aliases() {
        let png_low_q = TransformSpec {
            dimensions: TargetDimensions::new(
                Some(Dimension::new(Axis::Width, 800, DEFAULT_MAX_DIMENSION).unwrap()),
                None,
            ),
            format: OutputFormat::Png,
            quality: Quality::new(20).unwrap(),
            filter: None,
            ..TransformSpec::default()
        };
        let png_high_q = TransformSpec {
            quality: Quality::new(95).unwrap(),
            ..png_low_q.clone()
        };

        assert_eq!(
            png_low_q.canonical_representation(),
            png_high_q.canonical_representation()
        );
        assert_eq!(
            png_low_q.canonical_representation(),
            "v1|f=png|q=-|w=800|h=-|flt=auto"
        );

        let jpeg_spec = TransformSpec {
            dimensions: TargetDimensions::new(
                Some(Dimension::new(Axis::Width, 640, DEFAULT_MAX_DIMENSION).unwrap()),
                Some(Dimension::new(Axis::Height, 480, DEFAULT_MAX_DIMENSION).unwrap()),
            ),
            format: "jpg".parse().unwrap(),
            quality: Quality::DEFAULT,
            filter: Some(ResizeFilter::Lanczos),
            ..TransformSpec::default()
        };
        assert_eq!(
            jpeg_spec.canonical_representation(),
            "v1|f=jpeg|q=75|w=640|h=480|flt=lanczos"
        );
    }
}

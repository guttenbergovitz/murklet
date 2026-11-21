//! Geometry value types and pure crop/resize planning (`fit`, `gravity`,
//! `rotate`, `dpr`, and `bg` alpha-flattening colour).

use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

use crate::domain::error::DomainError;
use crate::domain::transform::{ImageDimensions, TargetDimensions};

/// Strategy for fitting a source image into requested target dimensions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FitMode {
    /// Stretch to exact `w x h` when both axes are given; preserve aspect ratio when one axis is given.
    #[default]
    Fill,
    /// Fit inside `w x h` while preserving aspect ratio (may upscale smaller sources).
    Contain,
    /// Fill `w x h` while preserving aspect ratio, cropping overflow according to [`Gravity`].
    Cover,
    /// Like [`Self::Contain`], but never upscales beyond the source image's dimensions.
    ScaleDown,
}

impl FitMode {
    /// Returns the canonical lowercase identifier for this fit mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Contain => "contain",
            Self::Cover => "cover",
            Self::ScaleDown => "scale-down",
        }
    }
}

impl fmt::Display for FitMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FitMode {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.to_ascii_lowercase().as_str() {
            "fill" => Ok(Self::Fill),
            "contain" => Ok(Self::Contain),
            "cover" => Ok(Self::Cover),
            "scale-down" | "scale_down" => Ok(Self::ScaleDown),
            _ => Err(DomainError::UnsupportedFitMode(raw.to_owned())),
        }
    }
}

/// Crop anchor or normalised focal point used by [`FitMode::Cover`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Gravity {
    /// Centred crop window (default).
    #[default]
    Center,
    /// Top-centre anchor.
    North,
    /// Bottom-centre anchor.
    South,
    /// Middle-right anchor.
    East,
    /// Middle-left anchor.
    West,
    /// Top-left anchor.
    NorthWest,
    /// Top-right anchor.
    NorthEast,
    /// Bottom-left anchor.
    SouthWest,
    /// Bottom-right anchor.
    SouthEast,
    /// Normalised focal point stored as integer milli-units (`0..=1000` for `0.0..=1.0`).
    FocalPoint {
        /// Horizontal focal coordinate (`0` = left edge, `1000` = right edge).
        x_milli: u16,
        /// Vertical focal coordinate (`0` = top edge, `1000` = bottom edge).
        y_milli: u16,
    },
}

impl Gravity {
    /// Returns `(x_milli, y_milli)` in `0..=1000` representing the focal anchor.
    #[must_use]
    pub const fn anchor_millis(self) -> (u16, u16) {
        match self {
            Self::Center => (500, 500),
            Self::North => (500, 0),
            Self::South => (500, 1000),
            Self::East => (1000, 500),
            Self::West => (0, 500),
            Self::NorthWest => (0, 0),
            Self::NorthEast => (1000, 0),
            Self::SouthWest => (0, 1000),
            Self::SouthEast => (1000, 1000),
            Self::FocalPoint { x_milli, y_milli } => (x_milli, y_milli),
        }
    }

    /// Returns the canonical string form used in cache keys and diagnostics.
    #[must_use]
    pub fn canonical_str(self) -> String {
        match self {
            Self::Center => "center".to_owned(),
            Self::North => "north".to_owned(),
            Self::South => "south".to_owned(),
            Self::East => "east".to_owned(),
            Self::West => "west".to_owned(),
            Self::NorthWest => "nw".to_owned(),
            Self::NorthEast => "ne".to_owned(),
            Self::SouthWest => "sw".to_owned(),
            Self::SouthEast => "se".to_owned(),
            Self::FocalPoint { x_milli, y_milli } => format!("fp:{x_milli}:{y_milli}"),
        }
    }
}

impl fmt::Display for Gravity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical_str())
    }
}

impl FromStr for Gravity {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let lower = raw.to_ascii_lowercase();
        match lower.as_str() {
            "center" | "centre" | "ce" => Ok(Self::Center),
            "north" | "no" | "top" => Ok(Self::North),
            "south" | "so" | "bottom" => Ok(Self::South),
            "east" | "ea" | "right" => Ok(Self::East),
            "west" | "we" | "left" => Ok(Self::West),
            "nw" | "northwest" | "north-west" | "north_west" => Ok(Self::NorthWest),
            "ne" | "northeast" | "north-east" | "north_east" => Ok(Self::NorthEast),
            "sw" | "southwest" | "south-west" | "south_west" => Ok(Self::SouthWest),
            "se" | "southeast" | "south-east" | "south_east" => Ok(Self::SouthEast),
            _ => {
                if let Some(rest) = lower.strip_prefix("fp:")
                    && let Some((x_raw, y_raw)) = rest.split_once(':')
                    && let (Some(x_milli), Some(y_milli)) =
                        (parse_unit_millis(x_raw), parse_unit_millis(y_raw))
                {
                    return Ok(Self::FocalPoint { x_milli, y_milli });
                }
                Err(DomainError::UnsupportedGravity(raw.to_owned()))
            }
        }
    }
}

/// Parses a decimal number in `0.0..=1.0` (up to 3 decimal places) into `0..=1000`.
fn parse_unit_millis(raw: &str) -> Option<u16> {
    if raw == "1" || raw == "1.0" || raw == "1.00" || raw == "1.000" {
        return Some(1000);
    }
    if raw == "0" {
        return Some(0);
    }
    let frac = raw.strip_prefix("0.")?;
    if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let mut millis: u16 = 0;
    let mut place: u16 = 100;
    for b in frac.bytes() {
        millis += u16::from(b - b'0') * place;
        place /= 10;
    }
    Some(millis)
}

/// Clockwise lossless rotation in 90-degree increments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Rotation {
    /// No rotation (`0` degrees).
    #[default]
    Deg0,
    /// 90 degrees clockwise.
    Deg90,
    /// 180 degrees.
    Deg180,
    /// 270 degrees clockwise (90 degrees counter-clockwise).
    Deg270,
}

impl Rotation {
    /// Constructs a [`Rotation`] from a degree value (`0`, `90`, `180`, or `270`).
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::UnsupportedRotation`] for any other angle.
    pub const fn from_degrees(degrees: u16) -> Result<Self, DomainError> {
        match degrees {
            0 => Ok(Self::Deg0),
            90 => Ok(Self::Deg90),
            180 => Ok(Self::Deg180),
            270 => Ok(Self::Deg270),
            _ => Err(DomainError::UnsupportedRotation(degrees)),
        }
    }

    /// Returns the rotation angle in degrees.
    #[must_use]
    pub const fn degrees(self) -> u16 {
        match self {
            Self::Deg0 => 0,
            Self::Deg90 => 90,
            Self::Deg180 => 180,
            Self::Deg270 => 270,
        }
    }
}

impl fmt::Display for Rotation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.degrees())
    }
}

/// Validated Device Pixel Ratio (`1.00..=3.00`), stored as integer hundredths (`100..=300`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Dpr(u16);

impl Dpr {
    /// Default device pixel ratio (`1.0`).
    pub const ONE: Self = Self(100);

    /// Scales a raw target pixel dimension by this DPR, rounding to the nearest pixel.
    #[must_use]
    pub const fn scale_dimension(self, px: u32) -> u32 {
        let scaled = (px as u64 * self.0 as u64 + 50) / 100;
        if scaled > u32::MAX as u64 {
            u32::MAX
        } else {
            #[allow(clippy::cast_possible_truncation)]
            {
                scaled as u32
            }
        }
    }

    /// Returns the DPR in hundredths (`100..=300`).
    #[must_use]
    pub const fn as_hundredths(self) -> u16 {
        self.0
    }
}

impl Default for Dpr {
    fn default() -> Self {
        Self::ONE
    }
}

impl FromStr for Dpr {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let hundredths =
            parse_dpr_hundredths(raw).ok_or_else(|| DomainError::InvalidDpr(raw.to_owned()))?;
        if !(100..=300).contains(&hundredths) {
            return Err(DomainError::InvalidDpr(raw.to_owned()));
        }
        Ok(Self(hundredths))
    }
}

fn parse_dpr_hundredths(raw: &str) -> Option<u16> {
    if let Some((whole_str, frac_str)) = raw.split_once('.') {
        if whole_str.is_empty()
            || frac_str.is_empty()
            || frac_str.len() > 2
            || !whole_str.bytes().all(|b| b.is_ascii_digit())
            || !frac_str.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let whole = whole_str.parse::<u16>().ok()?;
        let frac = if frac_str.len() == 1 {
            frac_str.parse::<u16>().ok()? * 10
        } else {
            frac_str.parse::<u16>().ok()?
        };
        whole.checked_mul(100)?.checked_add(frac)
    } else {
        if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        raw.parse::<u16>().ok()?.checked_mul(100)
    }
}

/// RGB background colour used to flatten alpha channels onto opaque formats such as JPEG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BackgroundColour {
    /// Red channel (`0..=255`).
    pub r: u8,
    /// Green channel (`0..=255`).
    pub g: u8,
    /// Blue channel (`0..=255`).
    pub b: u8,
}

impl BackgroundColour {
    /// Returns the canonical 6-character lowercase hexadecimal string (without `#`).
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl fmt::Display for BackgroundColour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl FromStr for BackgroundColour {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let hex = raw.strip_prefix('#').unwrap_or(raw);
        match hex.len() {
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16)
                    .map_err(|_| DomainError::InvalidBackground(raw.to_owned()))?;
                let g = u8::from_str_radix(&hex[2..4], 16)
                    .map_err(|_| DomainError::InvalidBackground(raw.to_owned()))?;
                let b = u8::from_str_radix(&hex[4..6], 16)
                    .map_err(|_| DomainError::InvalidBackground(raw.to_owned()))?;
                Ok(Self { r, g, b })
            }
            3 => {
                let r = u8::from_str_radix(&hex[0..1], 16)
                    .map_err(|_| DomainError::InvalidBackground(raw.to_owned()))?;
                let g = u8::from_str_radix(&hex[1..2], 16)
                    .map_err(|_| DomainError::InvalidBackground(raw.to_owned()))?;
                let b = u8::from_str_radix(&hex[2..3], 16)
                    .map_err(|_| DomainError::InvalidBackground(raw.to_owned()))?;
                Ok(Self {
                    r: r * 17,
                    g: g * 17,
                    b: b * 17,
                })
            }
            _ => Err(DomainError::InvalidBackground(raw.to_owned())),
        }
    }
}

/// Source pixel rectangle cropped before resampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CropRect {
    /// Left offset in pixels.
    pub x: u32,
    /// Top offset in pixels.
    pub y: u32,
    /// Crop width in pixels (`>= 1`).
    pub width: u32,
    /// Crop height in pixels (`>= 1`).
    pub height: u32,
}

/// Pure, deterministic crop-and-resize plan computed from source and target geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResizePlan {
    /// Optional crop rectangle applied to the source image before resampling.
    pub crop: Option<CropRect>,
    /// Final output dimensions after resampling.
    pub output: ImageDimensions,
}

/// Computes the exact [`ResizePlan`] for a source image, target bounds, [`FitMode`], and [`Gravity`].
#[must_use]
pub fn plan_resize(
    source: ImageDimensions,
    target: TargetDimensions,
    fit: FitMode,
    gravity: Gravity,
) -> ResizePlan {
    match fit {
        FitMode::Fill => ResizePlan {
            crop: None,
            output: target.resolve(source),
        },
        FitMode::Contain => ResizePlan {
            crop: None,
            output: resolve_contain(source, target, false),
        },
        FitMode::ScaleDown => ResizePlan {
            crop: None,
            output: resolve_contain(source, target, true),
        },
        FitMode::Cover => resolve_cover(source, target, gravity),
    }
}

fn resolve_contain(
    source: ImageDimensions,
    target: TargetDimensions,
    prevent_upscale: bool,
) -> ImageDimensions {
    let (sw, sh) = (source.width(), source.height());

    let resolved = match (target.width(), target.height()) {
        (None, None) => source,
        (Some(_), None) | (None, Some(_)) => target.resolve(source),
        (Some(tw), Some(th)) => {
            let (tw, th) = (tw.get(), th.get());
            // Compare sw/sh vs tw/th using exact cross-multiplication in u64.
            if u64::from(sw) * u64::from(th) >= u64::from(tw) * u64::from(sh) {
                // Source is wider than target box: width is the constraining axis.
                let h = scale_u32(sh, tw, sw);
                make_dims(tw, h)
            } else {
                // Source is taller than target box: height is the constraining axis.
                let w = scale_u32(sw, th, sh);
                make_dims(w, th)
            }
        }
    };

    if prevent_upscale && (resolved.width() > sw || resolved.height() > sh) {
        source
    } else {
        resolved
    }
}

fn resolve_cover(
    source: ImageDimensions,
    target: TargetDimensions,
    gravity: Gravity,
) -> ResizePlan {
    let (Some(tw), Some(th)) = (target.width(), target.height()) else {
        return ResizePlan {
            crop: None,
            output: target.resolve(source),
        };
    };

    let (sw, sh) = (source.width(), source.height());
    let (tw, th) = (tw.get(), th.get());
    let output = make_dims(tw, th);

    // Find the largest crop rectangle inside `source` matching aspect ratio `tw / th`.
    let (crop_w, crop_h) =
        match (u64::from(sw) * u64::from(th)).cmp(&(u64::from(tw) * u64::from(sh))) {
            std::cmp::Ordering::Greater => {
                // Source is wider than target: crop horizontally.
                (scale_u32(sh, tw, th).min(sw), sh)
            }
            std::cmp::Ordering::Less => {
                // Source is taller than target: crop vertically.
                (sw, scale_u32(sw, th, tw).min(sh))
            }
            std::cmp::Ordering::Equal => return ResizePlan { crop: None, output },
        };

    let (x, y) = compute_crop_origin(sw, sh, crop_w, crop_h, gravity);
    let crop = if crop_w == sw && crop_h == sh {
        None
    } else {
        Some(CropRect {
            x,
            y,
            width: crop_w,
            height: crop_h,
        })
    };

    ResizePlan { crop, output }
}

fn compute_crop_origin(
    src_w: u32,
    src_h: u32,
    crop_w: u32,
    crop_h: u32,
    gravity: Gravity,
) -> (u32, u32) {
    let max_x = src_w.saturating_sub(crop_w);
    let max_y = src_h.saturating_sub(crop_h);

    if let Gravity::FocalPoint { x_milli, y_milli } = gravity {
        // Centre the crop window around the focal pixel, clamped within [0, max].
        let focal_x = (u64::from(src_w) * u64::from(x_milli)) / 1000;
        let focal_y = (u64::from(src_h) * u64::from(y_milli)) / 1000;
        let desired_x = focal_x.saturating_sub(u64::from(crop_w) / 2);
        let desired_y = focal_y.saturating_sub(u64::from(crop_h) / 2);
        let x = u32::try_from(desired_x).unwrap_or(max_x).min(max_x);
        let y = u32::try_from(desired_y).unwrap_or(max_y).min(max_y);
        (x, y)
    } else {
        let (ax, ay) = gravity.anchor_millis();
        let x = u32::try_from((u64::from(max_x) * u64::from(ax)) / 1000)
            .unwrap_or(max_x)
            .min(max_x);
        let y = u32::try_from((u64::from(max_y) * u64::from(ay)) / 1000)
            .unwrap_or(max_y)
            .min(max_y);
        (x, y)
    }
}

fn scale_u32(a: u32, b: u32, denom: u32) -> u32 {
    let scaled = (u64::from(a) * u64::from(b)) / u64::from(denom);
    u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
}

fn make_dims(w: u32, h: u32) -> ImageDimensions {
    let nw = NonZeroU32::new(w).unwrap_or(NonZeroU32::MIN);
    let nh = NonZeroU32::new(h).unwrap_or(NonZeroU32::MIN);
    ImageDimensions::from_non_zero(nw, nh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Axis, DEFAULT_MAX_DIMENSION, Dimension};

    fn target(w: u32, h: u32) -> TargetDimensions {
        TargetDimensions::new(
            Some(Dimension::new(Axis::Width, w, DEFAULT_MAX_DIMENSION).unwrap()),
            Some(Dimension::new(Axis::Height, h, DEFAULT_MAX_DIMENSION).unwrap()),
        )
    }

    #[test]
    fn contain_and_scale_down_preserve_aspect_ratio_and_upscale_rules() {
        let wide = ImageDimensions::new(1600, 800).unwrap();
        let plan_contain = plan_resize(wide, target(400, 400), FitMode::Contain, Gravity::Center);
        assert_eq!(plan_contain.crop, None);
        assert_eq!(plan_contain.output, ImageDimensions::new(400, 200).unwrap());

        let small = ImageDimensions::new(200, 100).unwrap();
        let upscaled = plan_resize(small, target(400, 400), FitMode::Contain, Gravity::Center);
        assert_eq!(upscaled.output, ImageDimensions::new(400, 200).unwrap());

        let no_upscale = plan_resize(small, target(400, 400), FitMode::ScaleDown, Gravity::Center);
        assert_eq!(no_upscale.output, small);
    }

    #[test]
    fn cover_computes_centred_anchored_and_focal_point_crops() {
        let wide = ImageDimensions::new(1000, 500).unwrap();

        let centred = plan_resize(wide, target(200, 200), FitMode::Cover, Gravity::Center);
        assert_eq!(
            centred.crop,
            Some(CropRect {
                x: 250,
                y: 0,
                width: 500,
                height: 500,
            })
        );
        assert_eq!(centred.output, ImageDimensions::new(200, 200).unwrap());

        let west = plan_resize(wide, target(200, 200), FitMode::Cover, Gravity::West);
        assert_eq!(west.crop.unwrap().x, 0);

        let east = plan_resize(wide, target(200, 200), FitMode::Cover, Gravity::East);
        assert_eq!(east.crop.unwrap().x, 500);

        let focal = plan_resize(
            wide,
            target(200, 200),
            FitMode::Cover,
            "fp:0.8:0.5".parse().unwrap(),
        );
        assert_eq!(focal.crop.unwrap().x, 500);
    }

    #[test]
    fn parses_dpr_background_colour_rotation_and_gravity() {
        let dpr: Dpr = "2.5".parse().unwrap();
        assert_eq!(dpr.scale_dimension(400), 1000);
        assert!("0.5".parse::<Dpr>().is_err());
        assert!("4".parse::<Dpr>().is_err());

        let white: BackgroundColour = "#fff".parse().unwrap();
        assert_eq!(
            white,
            BackgroundColour {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(white.to_hex(), "ffffff");
        assert_eq!(
            "1a2b3c".parse::<BackgroundColour>().unwrap(),
            BackgroundColour {
                r: 0x1a,
                g: 0x2b,
                b: 0x3c
            }
        );
        assert!("zzzzzz".parse::<BackgroundColour>().is_err());

        assert_eq!(Rotation::from_degrees(90).unwrap(), Rotation::Deg90);
        assert!(Rotation::from_degrees(45).is_err());

        assert_eq!(
            "fp:0.25:0.75".parse::<Gravity>().unwrap(),
            Gravity::FocalPoint {
                x_milli: 250,
                y_milli: 750
            }
        );
    }
}

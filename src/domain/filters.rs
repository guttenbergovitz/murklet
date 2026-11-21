//! Validated post-resize pixel filters and mirror-flip value types.
//!
//! All numeric parameters are bounded and stored as integers so filter specs
//! remain `Copy`, `Eq`, `Hash`, and immune to CPU-exhaustion abuse.

use std::fmt;
use std::fmt::Write as _;
use std::str::FromStr;

use crate::domain::error::DomainError;

/// Mirror flip applied alongside rotation before crop/resize.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Flip {
    /// No flip.
    #[default]
    None,
    /// Horizontal mirror flip (`h`).
    Horizontal,
    /// Vertical mirror flip (`v`).
    Vertical,
    /// Both horizontal and vertical flip (`hv`).
    Both,
}

impl Flip {
    /// Returns the canonical lowercase identifier, or `None` when disabled.
    #[must_use]
    pub const fn as_str(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Horizontal => Some("h"),
            Self::Vertical => Some("v"),
            Self::Both => Some("hv"),
        }
    }
}

impl FromStr for Flip {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.to_ascii_lowercase().as_str() {
            "h" | "horizontal" => Ok(Self::Horizontal),
            "v" | "vertical" => Ok(Self::Vertical),
            "hv" | "vh" | "both" => Ok(Self::Both),
            _ => Err(DomainError::UnsupportedFlip(raw.to_owned())),
        }
    }
}

/// Validated blur sigma in `0.5..=20.0`, stored in integer tenths (`5..=200`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlurSigma(u16);

impl BlurSigma {
    /// Returns the blur sigma as `f32` for the image engine.
    #[must_use]
    pub fn as_f32(self) -> f32 {
        f32::from(self.0) / 10.0
    }

    /// Returns the sigma in integer tenths (`5..=200`).
    #[must_use]
    pub const fn as_tenths(self) -> u16 {
        self.0
    }
}

impl FromStr for BlurSigma {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let tenths = parse_tenths(raw).ok_or_else(|| DomainError::InvalidBlur(raw.to_owned()))?;
        if !(5..=200).contains(&tenths) {
            return Err(DomainError::InvalidBlur(raw.to_owned()));
        }
        Ok(Self(tenths))
    }
}

/// Validated sharpen sigma in `0.5..=10.0`, stored in integer tenths (`5..=100`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SharpenSigma(u16);

impl SharpenSigma {
    /// Returns the sharpen sigma as `f32` for the image engine.
    #[must_use]
    pub fn as_f32(self) -> f32 {
        f32::from(self.0) / 10.0
    }

    /// Returns the sigma in integer tenths (`5..=100`).
    #[must_use]
    pub const fn as_tenths(self) -> u16 {
        self.0
    }
}

impl FromStr for SharpenSigma {
    type Err = DomainError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let tenths =
            parse_tenths(raw).ok_or_else(|| DomainError::InvalidSharpen(raw.to_owned()))?;
        if !(5..=100).contains(&tenths) {
            return Err(DomainError::InvalidSharpen(raw.to_owned()));
        }
        Ok(Self(tenths))
    }
}

/// Validated brightness offset in `-100..=100` (`0` = unchanged).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Brightness(i8);

impl Brightness {
    /// Constructs a validated brightness adjustment in `-100..=100`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::InvalidBrightness`] when outside `-100..=100`.
    pub const fn new(value: i16) -> Result<Self, DomainError> {
        if value < -100 || value > 100 {
            return Err(DomainError::InvalidBrightness(value));
        }
        #[allow(clippy::cast_possible_truncation)]
        Ok(Self(value as i8))
    }

    /// Returns the signed brightness offset (`-100..=100`).
    #[must_use]
    pub const fn get(self) -> i8 {
        self.0
    }
}

/// Validated contrast adjustment in `-100..=100` (`0` = unchanged).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Contrast(i8);

impl Contrast {
    /// Constructs a validated contrast adjustment in `-100..=100`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::InvalidContrast`] when outside `-100..=100`.
    pub const fn new(value: i16) -> Result<Self, DomainError> {
        if value < -100 || value > 100 {
            return Err(DomainError::InvalidContrast(value));
        }
        #[allow(clippy::cast_possible_truncation)]
        Ok(Self(value as i8))
    }

    /// Returns the signed contrast adjustment (`-100..=100`).
    #[must_use]
    pub const fn get(self) -> i8 {
        self.0
    }
}

/// Canonical set of image filters executed in a deterministic pipeline order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ImageFilters {
    /// Optional mirror flip (applied before crop/resize).
    pub flip: Flip,
    /// Convert image to greyscale.
    pub grayscale: bool,
    /// Invert RGB channels (photographic negative).
    pub invert: bool,
    /// Optional linear-time blur sigma (`0.5..=20.0`).
    pub blur: Option<BlurSigma>,
    /// Optional unsharp-mask sharpen sigma (`0.5..=10.0`).
    pub sharpen: Option<SharpenSigma>,
    /// Brightness adjustment (`-100..=100`).
    pub brightness: Brightness,
    /// Contrast adjustment (`-100..=100`).
    pub contrast: Contrast,
}

impl ImageFilters {
    /// Reports whether all filters are inactive.
    #[must_use]
    pub const fn is_identity(&self) -> bool {
        matches!(self.flip, Flip::None)
            && !self.grayscale
            && !self.invert
            && self.blur.is_none()
            && self.sharpen.is_none()
            && self.brightness.get() == 0
            && self.contrast.get() == 0
    }

    /// Appends canonical filter segments to a cache-key representation string.
    pub fn append_canonical(&self, out: &mut String) {
        if let Some(flip) = self.flip.as_str() {
            let _ = write!(out, "|flip={flip}");
        }
        if self.grayscale {
            out.push_str("|gs=1");
        }
        if self.invert {
            out.push_str("|inv=1");
        }
        if let Some(blur) = self.blur {
            let _ = write!(out, "|blur={}", blur.as_tenths());
        }
        if let Some(sharpen) = self.sharpen {
            let _ = write!(out, "|sharp={}", sharpen.as_tenths());
        }
        if self.brightness.get() != 0 {
            let _ = write!(out, "|bri={}", self.brightness.get());
        }
        if self.contrast.get() != 0 {
            let _ = write!(out, "|con={}", self.contrast.get());
        }
    }
}

impl fmt::Display for ImageFilters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_identity() {
            return f.write_str("none");
        }
        let mut parts = Vec::new();
        if let Some(flip) = self.flip.as_str() {
            parts.push(format!("flip={flip}"));
        }
        if self.grayscale {
            parts.push("grayscale".to_owned());
        }
        if self.invert {
            parts.push("invert".to_owned());
        }
        if let Some(blur) = self.blur {
            parts.push(format!("blur={:.1}", blur.as_f32()));
        }
        if let Some(sharpen) = self.sharpen {
            parts.push(format!("sharpen={:.1}", sharpen.as_f32()));
        }
        if self.brightness.get() != 0 {
            parts.push(format!("brightness={}", self.brightness.get()));
        }
        if self.contrast.get() != 0 {
            parts.push(format!("contrast={}", self.contrast.get()));
        }
        f.write_str(&parts.join(","))
    }
}

/// Parses a non-negative decimal with at most one fractional digit into tenths.
fn parse_tenths(raw: &str) -> Option<u16> {
    if let Some((whole_str, frac_str)) = raw.split_once('.') {
        if whole_str.is_empty()
            || frac_str.len() != 1
            || !whole_str.bytes().all(|b| b.is_ascii_digit())
            || !frac_str.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let whole = whole_str.parse::<u16>().ok()?;
        let frac = frac_str.parse::<u16>().ok()?;
        whole.checked_mul(10)?.checked_add(frac)
    } else {
        if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        raw.parse::<u16>().ok()?.checked_mul(10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_filter_bounds_and_canonical_output() {
        assert_eq!("h".parse::<Flip>().unwrap(), Flip::Horizontal);
        assert_eq!("vh".parse::<Flip>().unwrap(), Flip::Both);
        assert!("diag".parse::<Flip>().is_err());

        assert_eq!("2.5".parse::<BlurSigma>().unwrap().as_tenths(), 25);
        assert!("0.1".parse::<BlurSigma>().is_err());
        assert!("25".parse::<BlurSigma>().is_err());

        assert_eq!("1.5".parse::<SharpenSigma>().unwrap().as_tenths(), 15);
        assert!("12".parse::<SharpenSigma>().is_err());

        assert_eq!(Brightness::new(-50).unwrap().get(), -50);
        assert!(Brightness::new(101).is_err());
        assert_eq!(Contrast::new(40).unwrap().get(), 40);
        assert!(Contrast::new(-101).is_err());

        let filters = ImageFilters {
            flip: Flip::Horizontal,
            grayscale: true,
            invert: true,
            blur: Some("2.5".parse().unwrap()),
            sharpen: Some("1.5".parse().unwrap()),
            brightness: Brightness::new(10).unwrap(),
            contrast: Contrast::new(-5).unwrap(),
        };
        let mut canonical = String::new();
        filters.append_canonical(&mut canonical);
        assert_eq!(
            canonical,
            "|flip=h|gs=1|inv=1|blur=25|sharp=15|bri=10|con=-5"
        );
    }
}

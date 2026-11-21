//! Validated policy-controlled watermark specification value types.
//!
//! Watermarks are referenced solely by logical name configured under
//! `[watermarks]` in `murklet.toml`, preventing arbitrary URL or path injection.

use std::fmt;
use std::fmt::Write as _;

use crate::domain::error::DomainError;
use crate::domain::geometry::Gravity;

/// Validated watermark opacity percentage in `1..=100` (default `50`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WatermarkOpacity(u8);

impl WatermarkOpacity {
    /// Default watermark opacity (`50%`).
    pub const DEFAULT: Self = Self(50);

    /// Constructs a validated opacity percentage in `1..=100`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::InvalidWatermarkOpacity`] when outside `1..=100`.
    pub const fn new(value: u8) -> Result<Self, DomainError> {
        if value == 0 || value > 100 {
            return Err(DomainError::InvalidWatermarkOpacity(value));
        }
        Ok(Self(value))
    }

    /// Returns the opacity percentage (`1..=100`).
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for WatermarkOpacity {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Validated watermark width scale as a percentage of the target image width (`5..=100`, default `20`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WatermarkScale(u8);

impl WatermarkScale {
    /// Default watermark width scale (`20%` of target image width).
    pub const DEFAULT: Self = Self(20);

    /// Constructs a validated scale percentage in `5..=100`.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::InvalidWatermarkScale`] when outside `5..=100`.
    pub const fn new(value: u8) -> Result<Self, DomainError> {
        if value < 5 || value > 100 {
            return Err(DomainError::InvalidWatermarkScale(value));
        }
        Ok(Self(value))
    }

    /// Returns the width scale percentage (`5..=100`).
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for WatermarkScale {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Validated specification for compositing a named watermark over the target image.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatermarkSpec {
    /// Logical watermark name declared in `[watermarks]`.
    pub name: String,
    /// Anchor position or focal point (default `south-east`).
    pub position: Gravity,
    /// Opacity percentage (`1..=100`, default `50`).
    pub opacity: WatermarkOpacity,
    /// Width percentage relative to target image width (`5..=100`, default `20`).
    pub scale: WatermarkScale,
}

impl WatermarkSpec {
    /// Constructs a validated [`WatermarkSpec`].
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::InvalidWatermarkName`] if `name` is empty or contains
    /// characters outside ASCII letters, digits, `-`, and `_`.
    pub fn new(
        name: impl Into<String>,
        position: Gravity,
        opacity: WatermarkOpacity,
        scale: WatermarkScale,
    ) -> Result<Self, DomainError> {
        let name = name.into();
        if !is_valid_watermark_name(&name) {
            return Err(DomainError::InvalidWatermarkName(name));
        }
        Ok(Self {
            name,
            position,
            opacity,
            scale,
        })
    }

    /// Appends canonical watermark parameters to a cache-key representation string.
    pub fn append_canonical(&self, out: &mut String) {
        let _ = write!(
            out,
            "|wm={}:{}:{}:{}",
            self.name,
            self.position.canonical_str(),
            self.opacity.get(),
            self.scale.get()
        );
    }
}

impl fmt::Display for WatermarkSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}@{}({}%,{}%)",
            self.name,
            self.position,
            self.opacity.get(),
            self.scale.get()
        )
    }
}

/// Reports whether `name` is a valid logical watermark identifier (`[A-Za-z0-9_-]+`).
#[must_use]
pub fn is_valid_watermark_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_watermark_opacity_and_scale_bounds() {
        assert_eq!(WatermarkOpacity::default().get(), 50);
        assert_eq!(
            WatermarkOpacity::new(0),
            Err(DomainError::InvalidWatermarkOpacity(0))
        );
        assert_eq!(
            WatermarkOpacity::new(101),
            Err(DomainError::InvalidWatermarkOpacity(101))
        );
        assert_eq!(WatermarkOpacity::new(1).unwrap().get(), 1);
        assert_eq!(WatermarkOpacity::new(100).unwrap().get(), 100);

        assert_eq!(WatermarkScale::default().get(), 20);
        assert_eq!(
            WatermarkScale::new(4),
            Err(DomainError::InvalidWatermarkScale(4))
        );
        assert_eq!(
            WatermarkScale::new(101),
            Err(DomainError::InvalidWatermarkScale(101))
        );
        assert_eq!(WatermarkScale::new(5).unwrap().get(), 5);
        assert_eq!(WatermarkScale::new(100).unwrap().get(), 100);
    }

    #[test]
    fn validates_watermark_spec_names_and_canonical_representation() {
        assert!(
            WatermarkSpec::new(
                "",
                Gravity::SouthEast,
                WatermarkOpacity::DEFAULT,
                WatermarkScale::DEFAULT
            )
            .is_err()
        );
        assert!(
            WatermarkSpec::new(
                "../logo",
                Gravity::SouthEast,
                WatermarkOpacity::DEFAULT,
                WatermarkScale::DEFAULT
            )
            .is_err()
        );
        assert!(
            WatermarkSpec::new(
                "https://evil.com/a.png",
                Gravity::SouthEast,
                WatermarkOpacity::DEFAULT,
                WatermarkScale::DEFAULT
            )
            .is_err()
        );

        let spec = WatermarkSpec::new(
            "brand_logo-1",
            Gravity::SouthEast,
            WatermarkOpacity::new(40).unwrap(),
            WatermarkScale::new(25).unwrap(),
        )
        .unwrap();

        let mut canonical = String::new();
        spec.append_canonical(&mut canonical);
        assert_eq!(canonical, "|wm=brand_logo-1:se:40:25");
        assert_eq!(spec.to_string(), "brand_logo-1@se(40%,25%)");
    }
}

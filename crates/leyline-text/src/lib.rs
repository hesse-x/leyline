//! Bounded, UI-thread-owned terminal font and shaping facade.

#![deny(unsafe_code)]

mod ffi;

use std::{num::NonZeroU16, sync::Arc};

pub use ffi::TextSystem;

pub const MAX_FACES: usize = 256;
pub const MAX_GLYPH_BITMAP_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_GLYPH_BITMAPS: usize = 8192;
pub const MAX_PREPARED_GLYPHS: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FontStyle {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FontRequest {
    pub family: Arc<str>,
    pub style: FontStyle,
    pub logical_size_milli_pt: u32,
    pub scale_120: u32,
    pub ligatures: bool,
}

impl FontRequest {
    /// Converts the configuration boundary's point value exactly once.
    ///
    /// # Errors
    /// Returns [`TextError::InvalidRequest`] for invalid family, size, or scale values.
    pub fn from_points(
        family: impl Into<Arc<str>>,
        points: f64,
        scale_120: u32,
        ligatures: bool,
    ) -> Result<Self, TextError> {
        if !points.is_finite() || points <= 0.0 || points > 512.0 || scale_120 == 0 {
            return Err(TextError::InvalidRequest("font size or scale is invalid"));
        }
        let family = family.into();
        if family.is_empty() || family.len() > 1024 || family.contains('\0') {
            return Err(TextError::InvalidRequest("font family is invalid"));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let logical_size_milli_pt = (points * 1000.0).round() as u32;
        Ok(Self {
            family,
            style: FontStyle::Regular,
            logical_size_milli_pt,
            scale_120,
            ligatures,
        })
    }

    /// Returns a checked 26.6 physical size using Wayland as the only scale source.
    ///
    /// # Errors
    /// Returns a capacity error when rational arithmetic overflows.
    pub fn physical_size_26_6(&self) -> Result<i64, TextError> {
        let numerator = u64::from(self.logical_size_milli_pt)
            .checked_mul(96)
            .and_then(|value| value.checked_mul(u64::from(self.scale_120)))
            .and_then(|value| value.checked_mul(64))
            .ok_or(TextError::CapacityExceeded("font size"))?;
        let denominator = 1000_u64 * 72 * 120;
        let rounded = numerator
            .checked_add(denominator / 2)
            .ok_or(TextError::CapacityExceeded("font size"))?
            / denominator;
        i64::try_from(rounded).map_err(|_| TextError::CapacityExceeded("font size"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellMetrics {
    pub width_px: NonZeroU16,
    pub height_px: NonZeroU16,
    pub baseline_px: i16,
    pub underline_y_px: i16,
    pub underline_thickness_px: NonZeroU16,
    pub strike_y_px: i16,
    pub strike_thickness_px: NonZeroU16,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FaceId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GlyphKey {
    pub font_generation: u64,
    pub face: FaceId,
    pub glyph_id: u32,
    pub synthetic_bold: bool,
    pub synthetic_italic: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlyphBitmap {
    pub size_px: [u16; 2],
    pub bearing_px: [i16; 2],
    pub advance_26_6: i32,
    pub coverage: Arc<[u8]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlyphAsset {
    pub key: GlyphKey,
    pub bitmap: GlyphBitmap,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedGlyph {
    pub key: GlyphKey,
    pub cluster: u32,
    pub offset_26_6: [i32; 2],
    pub advance_26_6: [i32; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShapedCluster {
    pub glyphs: Vec<ShapedGlyph>,
    pub assets: Vec<GlyphAsset>,
}

#[derive(Debug, thiserror::Error)]
pub enum TextError {
    #[error("invalid font request: {0}")]
    InvalidRequest(&'static str),
    #[error("font environment error: {0}")]
    Environment(String),
    #[error("font data error: {0}")]
    FontData(String),
    #[error("text shaping error: {0}")]
    Shape(String),
    #[error("text resource exceeds its hard limit: {0}")]
    CapacityExceeded(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_size_uses_wayland_scale_once() {
        let one = FontRequest::from_points("monospace", 12.0, 120, false).unwrap();
        let two = FontRequest::from_points("monospace", 12.0, 240, false).unwrap();
        assert_eq!(one.physical_size_26_6().unwrap(), 16 * 64);
        assert_eq!(two.physical_size_26_6().unwrap(), 32 * 64);
    }

    #[test]
    fn invalid_font_boundaries_are_rejected() {
        assert!(FontRequest::from_points("", 12.0, 120, false).is_err());
        assert!(FontRequest::from_points("monospace", f64::NAN, 120, false).is_err());
        assert!(FontRequest::from_points("monospace", 12.0, 0, false).is_err());
    }

    #[test]
    fn system_facade_shapes_ascii_and_combining_cluster_when_available() {
        let request = FontRequest::from_points("monospace", 11.0, 120, false).unwrap();
        let mut system = TextSystem::new(request).unwrap();
        let shaped = system
            .shape_cluster("e\u{301}", FontStyle::Regular)
            .unwrap();
        assert!(!shaped.glyphs.is_empty());
        assert!(shaped.assets.iter().all(|asset| asset.bitmap.coverage.len()
            == usize::from(asset.bitmap.size_px[0]) * usize::from(asset.bitmap.size_px[1])));
    }
}

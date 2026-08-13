use std::collections::HashMap;

use leyline_text::{GlyphAsset, GlyphKey};

use crate::{GlyphInstance, GlyphPlacement};

pub const ATLAS_PAGE_SIZE: u16 = 2048;
pub const MAX_ATLAS_PAGES: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtlasRect {
    pub page: u16,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Default)]
struct ShelfPage {
    x: u16,
    y: u16,
    row_height: u16,
}

pub struct AtlasManager {
    pages: Vec<ShelfPage>,
    entries: HashMap<GlyphKey, AtlasRect>,
}

pub struct AtlasPreparation {
    pub uploads: Vec<(AtlasRect, GlyphAsset)>,
    pub instances: Vec<GlyphInstance>,
}

impl AtlasManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pages: vec![ShelfPage::default()],
            entries: HashMap::new(),
        }
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn prepare(
        &mut self,
        placements: &[GlyphPlacement],
        assets: &[GlyphAsset],
    ) -> Result<AtlasPreparation, AtlasError> {
        let mut uploads = Vec::new();
        for asset in assets {
            if asset.bitmap.size_px == [0, 0] || self.entries.contains_key(&asset.key) {
                continue;
            }
            let rect = self.allocate(asset.bitmap.size_px[0], asset.bitmap.size_px[1])?;
            self.entries.insert(asset.key, rect);
            uploads.push((rect, asset.clone()));
        }
        let mut instances = Vec::with_capacity(placements.len());
        for placement in placements {
            let Some(rect) = self.entries.get(&placement.key).copied() else {
                continue;
            };
            let asset = assets
                .iter()
                .find(|asset| asset.key == placement.key)
                .ok_or(AtlasError::MissingAsset)?;
            let bitmap = &asset.bitmap;
            let mut x = placement.origin_px[0];
            let mut y = placement.origin_px[1];
            let mut width = i32::from(bitmap.size_px[0]);
            let mut height = i32::from(bitmap.size_px[1]);
            let clip_x = i32::try_from(placement.clip_px[0]).map_err(|_| AtlasError::Overflow)?;
            let clip_y = i32::try_from(placement.clip_px[1]).map_err(|_| AtlasError::Overflow)?;
            let clip_right = clip_x.saturating_add(
                i32::try_from(placement.clip_px[2]).map_err(|_| AtlasError::Overflow)?,
            );
            let clip_bottom = clip_y.saturating_add(
                i32::try_from(placement.clip_px[3]).map_err(|_| AtlasError::Overflow)?,
            );
            let left_crop = (clip_x - x).clamp(0, width);
            let top_crop = (clip_y - y).clamp(0, height);
            x += left_crop;
            y += top_crop;
            width -= left_crop;
            height -= top_crop;
            width = width.min((clip_right - x).max(0));
            height = height.min((clip_bottom - y).max(0));
            if width == 0 || height == 0 {
                continue;
            }
            let atlas = f32::from(ATLAS_PAGE_SIZE);
            instances.push(GlyphInstance {
                origin_px: [x as f32, y as f32],
                size_px: [width as f32, height as f32],
                uv_min: [
                    f32::from(rect.x + u16::try_from(left_crop).map_err(|_| AtlasError::Overflow)?)
                        / atlas,
                    f32::from(rect.y + u16::try_from(top_crop).map_err(|_| AtlasError::Overflow)?)
                        / atlas,
                ],
                uv_max: [
                    f32::from(
                        rect.x
                            + u16::try_from(left_crop + width).map_err(|_| AtlasError::Overflow)?,
                    ) / atlas,
                    f32::from(
                        rect.y
                            + u16::try_from(top_crop + height).map_err(|_| AtlasError::Overflow)?,
                    ) / atlas,
                ],
                color: placement.color,
                atlas_page: rect.page,
            });
        }
        instances.sort_by_key(|glyph| glyph.atlas_page);
        Ok(AtlasPreparation { uploads, instances })
    }

    fn allocate(&mut self, width: u16, height: u16) -> Result<AtlasRect, AtlasError> {
        let padded_width = width.checked_add(2).ok_or(AtlasError::TooLarge)?;
        let padded_height = height.checked_add(2).ok_or(AtlasError::TooLarge)?;
        if padded_width > ATLAS_PAGE_SIZE || padded_height > ATLAS_PAGE_SIZE {
            return Err(AtlasError::TooLarge);
        }
        loop {
            let page_index = self.pages.len() - 1;
            let page = &mut self.pages[page_index];
            if u32::from(page.x) + u32::from(padded_width) > u32::from(ATLAS_PAGE_SIZE) {
                page.x = 0;
                page.y = page
                    .y
                    .checked_add(page.row_height)
                    .ok_or(AtlasError::Overflow)?;
                page.row_height = 0;
            }
            if u32::from(page.y) + u32::from(padded_height) <= u32::from(ATLAS_PAGE_SIZE) {
                let rect = AtlasRect {
                    page: u16::try_from(page_index).map_err(|_| AtlasError::Overflow)?,
                    x: page.x + 1,
                    y: page.y + 1,
                    width,
                    height,
                };
                page.x += padded_width;
                page.row_height = page.row_height.max(padded_height);
                return Ok(rect);
            }
            if self.pages.len() >= MAX_ATLAS_PAGES {
                return Err(AtlasError::Full);
            }
            self.pages.push(ShelfPage::default());
        }
    }
}

impl Default for AtlasManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AtlasError {
    #[error("glyph is too large for an atlas page")]
    TooLarge,
    #[error("all bounded atlas pages are full")]
    Full,
    #[error("atlas coordinate overflow")]
    Overflow,
    #[error("placement has no glyph asset")]
    MissingAsset,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allocation_includes_transparent_gutter() {
        let mut atlas = AtlasManager::new();
        let first = atlas.allocate(10, 10).unwrap();
        let second = atlas.allocate(10, 10).unwrap();
        assert_eq!(first.x, 1);
        assert_eq!(second.x, 13);
    }
    #[test]
    fn page_count_is_hard_bounded() {
        let mut atlas = AtlasManager::new();
        for _ in 0..MAX_ATLAS_PAGES {
            let _ = atlas.allocate(2046, 2046).unwrap();
        }
        assert!(matches!(atlas.allocate(2046, 2046), Err(AtlasError::Full)));
    }
}

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

#[derive(Clone, Default)]
struct ShelfPage {
    x: u16,
    y: u16,
    row_height: u16,
}

#[derive(Clone)]
pub struct AtlasManager {
    pages: Vec<ShelfPage>,
    entries: HashMap<GlyphKey, AtlasRect>,
    epoch: u64,
    repacks: u64,
}

pub struct AtlasPreparation {
    pub uploads: Vec<(AtlasRect, GlyphAsset)>,
    pub instances: Vec<GlyphInstance>,
    next: AtlasManager,
    repacked: bool,
}

impl AtlasPreparation {
    #[must_use]
    pub const fn is_repack(&self) -> bool {
        self.repacked
    }

    #[must_use]
    pub fn instances(&self) -> &[GlyphInstance] {
        &self.instances
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtlasStats {
    pub pages: usize,
    pub entries: usize,
    pub epoch: u64,
    pub repacks: u64,
}

impl AtlasManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pages: vec![ShelfPage::default()],
            entries: HashMap::new(),
            epoch: 0,
            repacks: 0,
        }
    }

    #[must_use]
    pub fn stats(&self) -> AtlasStats {
        AtlasStats {
            pages: self.pages.len(),
            entries: self.entries.len(),
            epoch: self.epoch,
            repacks: self.repacks,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn prepare(
        &self,
        placements: &[GlyphPlacement],
        assets: &[GlyphAsset],
    ) -> Result<AtlasPreparation, AtlasError> {
        let mut next = self.clone();
        let mut uploads = Vec::new();
        for asset in assets {
            validate_bitmap(asset)?;
            if asset.bitmap.size_px == [0, 0] || next.entries.contains_key(&asset.key) {
                continue;
            }
            let rect = match next.allocate(asset.bitmap.size_px[0], asset.bitmap.size_px[1]) {
                Ok(rect) => rect,
                Err(AtlasError::Full) => return self.prepare_repack(placements, assets),
                Err(error) => return Err(error),
            };
            next.entries.insert(asset.key, rect);
            uploads.push((rect, asset.clone()));
        }
        let instances = build_instances(&next.entries, placements, assets)?;
        Ok(AtlasPreparation {
            uploads,
            instances,
            next,
            repacked: false,
        })
    }

    fn prepare_repack(
        &self,
        placements: &[GlyphPlacement],
        assets: &[GlyphAsset],
    ) -> Result<AtlasPreparation, AtlasError> {
        let mut next = Self::new();
        next.epoch = self.epoch.checked_add(1).ok_or(AtlasError::Overflow)?;
        next.repacks = self.repacks.saturating_add(1);
        let mut uploads = Vec::new();
        for asset in assets {
            validate_bitmap(asset)?;
            if asset.bitmap.size_px == [0, 0] || next.entries.contains_key(&asset.key) {
                continue;
            }
            let rect = next.allocate(asset.bitmap.size_px[0], asset.bitmap.size_px[1])?;
            next.entries.insert(asset.key, rect);
            uploads.push((rect, asset.clone()));
        }
        let instances = build_instances(&next.entries, placements, assets)?;
        Ok(AtlasPreparation {
            uploads,
            instances,
            next,
            repacked: true,
        })
    }

    pub fn commit(&mut self, preparation: AtlasPreparation) -> AtlasCommit {
        let result = AtlasCommit {
            instances: preparation.instances,
            repacked: preparation.repacked,
        };
        *self = preparation.next;
        result
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

fn validate_bitmap(asset: &GlyphAsset) -> Result<(), AtlasError> {
    if asset.bitmap.format != leyline_text::GlyphFormat::Gray8 {
        return Err(AtlasError::UnsupportedFormat);
    }
    let expected = usize::from(asset.bitmap.size_px[0])
        .checked_mul(usize::from(asset.bitmap.size_px[1]))
        .ok_or(AtlasError::Overflow)?;
    if asset.bitmap.coverage.len() != expected {
        return Err(AtlasError::InvalidBitmap);
    }
    Ok(())
}

pub struct AtlasCommit {
    pub instances: Vec<GlyphInstance>,
    pub repacked: bool,
}

#[allow(clippy::cast_precision_loss)]
fn build_instances(
    entries: &HashMap<GlyphKey, AtlasRect>,
    placements: &[GlyphPlacement],
    assets: &[GlyphAsset],
) -> Result<Vec<GlyphInstance>, AtlasError> {
    let mut instances = Vec::with_capacity(placements.len());
    for placement in placements {
        let Some(rect) = entries.get(&placement.key).copied() else {
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
        let clip_right = clip_x
            .saturating_add(i32::try_from(placement.clip_px[2]).map_err(|_| AtlasError::Overflow)?);
        let clip_bottom = clip_y
            .saturating_add(i32::try_from(placement.clip_px[3]).map_err(|_| AtlasError::Overflow)?);
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
                    rect.x + u16::try_from(left_crop + width).map_err(|_| AtlasError::Overflow)?,
                ) / atlas,
                f32::from(
                    rect.y + u16::try_from(top_crop + height).map_err(|_| AtlasError::Overflow)?,
                ) / atlas,
            ],
            color: placement.color,
            atlas_page: rect.page,
        });
    }
    instances.sort_by_key(|glyph| glyph.atlas_page);
    Ok(instances)
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
    #[error("glyph bitmap length does not match its declared dimensions")]
    InvalidBitmap,
    #[error("glyph bitmap format is unsupported by the grayscale atlas")]
    UnsupportedFormat,
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_text::{FaceId, GlyphBitmap};
    use std::sync::Arc;

    fn asset(id: u32, size: [u16; 2]) -> GlyphAsset {
        GlyphAsset {
            key: GlyphKey {
                font_generation: 1,
                face: FaceId(0),
                glyph_id: id,
                synthetic_bold: false,
                synthetic_italic: false,
            },
            bitmap: GlyphBitmap {
                format: leyline_text::GlyphFormat::Gray8,
                size_px: size,
                bearing_px: [0, 0],
                advance_26_6: 64,
                coverage: Arc::from(vec![0; usize::from(size[0]) * usize::from(size[1])]),
            },
        }
    }

    fn placement(asset: &GlyphAsset) -> GlyphPlacement {
        GlyphPlacement {
            key: asset.key,
            origin_px: [0, 0],
            clip_px: [
                0,
                0,
                u32::from(asset.bitmap.size_px[0]),
                u32::from(asset.bitmap.size_px[1]),
            ],
            color: crate::LinearColor::from_srgba8(0xffff_ffff),
        }
    }
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

    #[test]
    fn prepare_is_atomic_until_explicit_commit() {
        let mut atlas = AtlasManager::new();
        let glyph = asset(1, [10, 10]);
        let prepared = atlas.prepare(&[placement(&glyph)], &[glyph]).unwrap();
        assert_eq!(atlas.stats().entries, 0);
        atlas.commit(prepared);
        assert_eq!(atlas.stats().entries, 1);
    }

    #[test]
    fn full_atlas_repacks_only_the_current_working_set() {
        let mut atlas = AtlasManager::new();
        let initial: Vec<_> = (0..MAX_ATLAS_PAGES)
            .map(|id| asset(u32::try_from(id).unwrap(), [2046, 2046]))
            .collect();
        let placements: Vec<_> = initial.iter().map(placement).collect();
        let prepared = atlas.prepare(&placements, &initial).unwrap();
        atlas.commit(prepared);
        assert_eq!(atlas.stats().pages, MAX_ATLAS_PAGES);

        let replacement = asset(99, [64, 64]);
        let prepared = atlas
            .prepare(&[placement(&replacement)], &[replacement])
            .unwrap();
        let commit = atlas.commit(prepared);
        assert!(commit.repacked);
        assert_eq!(atlas.stats().entries, 1);
        assert_eq!(atlas.stats().pages, 1);
        assert_eq!(atlas.stats().epoch, 1);
    }

    #[test]
    fn failed_repack_preserves_the_active_epoch_and_entries() {
        let mut atlas = AtlasManager::new();
        let initial = asset(1, [64, 64]);
        atlas.commit(
            atlas
                .prepare(&[placement(&initial)], std::slice::from_ref(&initial))
                .unwrap(),
        );
        let before = atlas.stats();
        let oversized: Vec<_> = (10..15).map(|id| asset(id, [2046, 2046])).collect();
        let placements: Vec<_> = oversized.iter().map(placement).collect();

        assert!(matches!(
            atlas.prepare(&placements, &oversized),
            Err(AtlasError::Full)
        ));
        assert_eq!(atlas.stats(), before);
    }
}

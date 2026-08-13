use std::num::NonZeroU16;

use leyline_gfx::{LogicalSize, PixelSize, Scale120};
use leyline_text::CellMetrics;

use crate::terminal::GridSize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GridLayout {
    pub viewport_px: PixelSize,
    pub content_origin_px: [u32; 2],
    pub cell_px: [NonZeroU16; 2],
    pub grid: GridSize,
    pub font_generation: u64,
}

impl GridLayout {
    #[must_use]
    pub fn cell_at_pixel(&self, pixel: [u32; 2]) -> Option<[u16; 2]> {
        let relative_x = pixel[0].checked_sub(self.content_origin_px[0])?;
        let relative_y = pixel[1].checked_sub(self.content_origin_px[1])?;
        let column = relative_x / u32::from(self.cell_px[0].get());
        let line = relative_y / u32::from(self.cell_px[1].get());
        if column >= u32::from(self.grid.columns.get()) || line >= u32::from(self.grid.lines.get())
        {
            return None;
        }
        Some([u16::try_from(column).ok()?, u16::try_from(line).ok()?])
    }

    /// Calculates a deterministic physical grid and centers its unused remainder.
    ///
    /// # Errors
    /// Returns [`LayoutError::Overflow`] when fixed-point scaling or bounds fail.
    #[allow(clippy::similar_names)]
    pub fn calculate(
        logical: LogicalSize,
        scale: Scale120,
        padding: [u16; 2],
        metrics: CellMetrics,
        font_generation: u64,
    ) -> Result<Self, LayoutError> {
        let viewport_px = scale.pixels(logical).map_err(|_| LayoutError::Overflow)?;
        let padding_px = scale
            .pixels(LogicalSize {
                width: u32::from(padding[0]).max(1),
                height: u32::from(padding[1]).max(1),
            })
            .map_err(|_| LayoutError::Overflow)?;
        let padding_x = if padding[0] == 0 { 0 } else { padding_px.width };
        let padding_y = if padding[1] == 0 {
            0
        } else {
            padding_px.height
        };
        let available_width = viewport_px
            .width
            .saturating_sub(padding_x.saturating_mul(2));
        let available_height = viewport_px
            .height
            .saturating_sub(padding_y.saturating_mul(2));
        let columns = (available_width / u32::from(metrics.width_px.get()))
            .clamp(1, u32::from(GridSize::MAX_COLUMNS));
        let lines = (available_height / u32::from(metrics.height_px.get()))
            .clamp(1, u32::from(GridSize::MAX_LINES));
        let grid = GridSize::new(
            u16::try_from(columns).map_err(|_| LayoutError::Overflow)?,
            u16::try_from(lines).map_err(|_| LayoutError::Overflow)?,
        )
        .map_err(|_| LayoutError::Overflow)?;
        let used_width = columns * u32::from(metrics.width_px.get());
        let used_height = lines * u32::from(metrics.height_px.get());
        Ok(Self {
            viewport_px,
            content_origin_px: [
                padding_x + available_width.saturating_sub(used_width) / 2,
                padding_y + available_height.saturating_sub(used_height) / 2,
            ],
            cell_px: [metrics.width_px, metrics.height_px],
            grid,
            font_generation,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("layout size or scale exceeds the supported range")]
    Overflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> CellMetrics {
        CellMetrics {
            width_px: NonZeroU16::new(9).unwrap(),
            height_px: NonZeroU16::new(18).unwrap(),
            baseline_px: 14,
            underline_y_px: 16,
            underline_thickness_px: NonZeroU16::new(1).unwrap(),
            strike_y_px: 9,
            strike_thickness_px: NonZeroU16::new(1).unwrap(),
        }
    }

    #[test]
    fn grid_is_centered_and_never_empty() {
        let tiny = GridLayout::calculate(
            LogicalSize {
                width: 1,
                height: 1,
            },
            Scale120::ONE,
            [8, 8],
            metrics(),
            1,
        )
        .unwrap();
        assert_eq!((tiny.grid.columns(), tiny.grid.lines()), (1, 1));
        let normal = GridLayout::calculate(
            LogicalSize {
                width: 800,
                height: 500,
            },
            Scale120::ONE,
            [8, 8],
            metrics(),
            1,
        )
        .unwrap();
        assert_eq!((normal.grid.columns(), normal.grid.lines()), (87, 26));
        assert_eq!(normal.content_origin_px, [8, 16]);
    }

    #[test]
    fn fractional_scale_changes_physical_grid() {
        let one = GridLayout::calculate(
            LogicalSize {
                width: 800,
                height: 500,
            },
            Scale120::ONE,
            [8, 8],
            metrics(),
            1,
        )
        .unwrap();
        let scaled = GridLayout::calculate(
            LogicalSize {
                width: 800,
                height: 500,
            },
            Scale120(150),
            [8, 8],
            metrics(),
            2,
        )
        .unwrap();
        assert!(scaled.grid.columns() > one.grid.columns());
        assert_eq!(scaled.font_generation, 2);
    }

    #[test]
    fn cell_mapping_uses_half_open_content_bounds() {
        let layout = GridLayout::calculate(
            LogicalSize {
                width: 100,
                height: 60,
            },
            Scale120::ONE,
            [5, 5],
            metrics(),
            1,
        )
        .unwrap();
        let origin = layout.content_origin_px;
        assert_eq!(layout.cell_at_pixel(origin), Some([0, 0]));
        assert_eq!(layout.cell_at_pixel([origin[0] - 1, origin[1]]), None);
        let right =
            origin[0] + u32::from(layout.grid.columns.get()) * u32::from(layout.cell_px[0].get());
        assert_eq!(layout.cell_at_pixel([right, origin[1]]), None);
    }
}

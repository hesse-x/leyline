use std::num::NonZeroU16;

use leyline_gfx::{LogicalSize, PixelSize, Scale120};
use leyline_text::CellMetrics;

use crate::terminal::GridSize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GridLayout {
    pub viewport_px: PixelSize,
    pub content_origin_px: [u32; 2],
    pub cell_px: [NonZeroU16; 2],
    pub cell_metrics: CellMetrics,
    pub grid: GridSize,
    pub font_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentInsets {
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
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

    /// Calculates a deterministic physical grid, left-aligning columns and centering rows.
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
        Self::calculate_with_style(
            logical,
            scale,
            ContentInsets {
                left: padding[0],
                right: padding[0],
                top: padding[1],
                bottom: padding[1],
            },
            metrics,
            0.0,
            font_generation,
        )
    }

    /// Calculates layout with asymmetric chrome insets and logical line spacing.
    ///
    /// # Errors
    /// Returns [`LayoutError::Overflow`] for invalid spacing, scale, or metric overflow.
    #[allow(clippy::similar_names)]
    pub fn calculate_with_style(
        logical: LogicalSize,
        scale: Scale120,
        insets: ContentInsets,
        metrics: CellMetrics,
        line_spacing: f64,
        font_generation: u64,
    ) -> Result<Self, LayoutError> {
        let metrics = metrics_with_line_spacing(metrics, line_spacing, scale)?;
        let viewport_px = scale.pixels(logical).map_err(|_| LayoutError::Overflow)?;
        let left = scale_inset(insets.left, scale)?;
        let right = scale_inset(insets.right, scale)?;
        let top = scale_inset(insets.top, scale)?;
        let bottom = scale_inset(insets.bottom, scale)?;
        let available_width = viewport_px.width.saturating_sub(left.saturating_add(right));
        let available_height = viewport_px
            .height
            .saturating_sub(top.saturating_add(bottom));
        let columns = (available_width / u32::from(metrics.width_px.get()))
            .clamp(1, u32::from(GridSize::MAX_COLUMNS));
        let lines = (available_height / u32::from(metrics.height_px.get()))
            .clamp(1, u32::from(GridSize::MAX_LINES));
        let grid = GridSize::new(
            u16::try_from(columns).map_err(|_| LayoutError::Overflow)?,
            u16::try_from(lines).map_err(|_| LayoutError::Overflow)?,
        )
        .map_err(|_| LayoutError::Overflow)?;
        let used_height = lines * u32::from(metrics.height_px.get());
        Ok(Self {
            viewport_px,
            content_origin_px: [left, top + available_height.saturating_sub(used_height) / 2],
            cell_px: [metrics.width_px, metrics.height_px],
            cell_metrics: metrics,
            grid,
            font_generation,
        })
    }
}

fn scale_inset(value: u16, scale: Scale120) -> Result<u32, LayoutError> {
    if value == 0 {
        return Ok(0);
    }
    scale
        .pixels(LogicalSize {
            width: u32::from(value),
            height: 1,
        })
        .map(|size| size.width)
        .map_err(|_| LayoutError::Overflow)
}

fn metrics_with_line_spacing(
    metrics: CellMetrics,
    logical_spacing: f64,
    scale: Scale120,
) -> Result<CellMetrics, LayoutError> {
    if !logical_spacing.is_finite() || !(0.0..=8.0).contains(&logical_spacing) {
        return Err(LayoutError::Overflow);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let spacing = (logical_spacing * f64::from(scale.0) / 120.0).round() as u16;
    let top = spacing / 2;
    let height = metrics
        .height_px
        .get()
        .checked_add(spacing)
        .ok_or(LayoutError::Overflow)?;
    let shift = i16::try_from(top).map_err(|_| LayoutError::Overflow)?;
    let max_y = i16::try_from(height.saturating_sub(1)).map_err(|_| LayoutError::Overflow)?;
    Ok(CellMetrics {
        width_px: metrics.width_px,
        height_px: NonZeroU16::new(height).ok_or(LayoutError::Overflow)?,
        baseline_px: metrics
            .baseline_px
            .checked_add(shift)
            .ok_or(LayoutError::Overflow)?,
        underline_y_px: metrics.underline_y_px.saturating_add(shift).clamp(0, max_y),
        underline_thickness_px: metrics.underline_thickness_px,
        strike_y_px: metrics.strike_y_px.saturating_add(shift).clamp(0, max_y),
        strike_thickness_px: metrics.strike_thickness_px,
    })
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
                width: 805,
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

    #[test]
    fn line_spacing_is_scaled_once_and_moves_the_baseline_by_the_top_half() {
        let layout = GridLayout::calculate_with_style(
            LogicalSize {
                width: 800,
                height: 500,
            },
            Scale120(180),
            ContentInsets {
                left: 8,
                right: 20,
                top: 8,
                bottom: 8,
            },
            metrics(),
            1.0,
            1,
        )
        .unwrap();
        assert_eq!(layout.cell_px[1].get(), 20);
        assert_eq!(layout.cell_metrics.baseline_px, 15);
        assert_eq!(layout.content_origin_px[0], 12);
    }
}

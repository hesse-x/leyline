use std::{collections::HashMap, sync::Arc};

use leyline_gfx::{GlyphPlacement, LinearColor, RectangleInstance, SceneData};
use leyline_text::{FontStyle, GlyphAsset, GlyphKey, TextError, TextSystem};

use crate::{
    config::{ColorsConfig, CursorStyle},
    layout::GridLayout,
    terminal::{CellWidth, FrameSnapshot, TerminalColor},
};

pub const MAX_UNIQUE_GLYPHS: usize = 65_536;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SelectionOverlay {
    pub snapshot_generation: u64,
    pub revision: u64,
    pub ranges: Arc<[CellRange]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellRange {
    pub start: [u16; 2],
    pub end: [u16; 2],
}

/// Converts one validated terminal snapshot into a bounded graphics scene.
///
/// # Errors
/// Returns a typed error for invalid snapshot contracts, font failures, or capacity limits.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]
pub fn compose(
    text: &mut TextSystem,
    snapshot: &FrameSnapshot,
    overlay: &SelectionOverlay,
    layout: &GridLayout,
    colors: &ColorsConfig,
    cursor_style: CursorStyle,
) -> Result<SceneData, ComposeError> {
    if snapshot.grid != layout.grid
        || snapshot.cells.len() != snapshot.grid.columns() * snapshot.grid.lines()
    {
        return Err(ComposeError::Snapshot("snapshot and layout grids differ"));
    }
    validate_widths(snapshot)?;
    let selected = |column: u16, line: u16| {
        overlay.snapshot_generation == snapshot.generation
            && overlay
                .ranges
                .iter()
                .any(|range| point_in_range([column, line], *range))
    };
    let mut rectangles = Vec::new();
    let mut glyphs = Vec::new();
    let mut assets = HashMap::<GlyphKey, GlyphAsset>::new();
    let metrics = text.metrics();
    for line in 0..snapshot.grid.lines() {
        let mut background_start = 0_usize;
        let mut background_color = None;
        for column in 0..snapshot.grid.columns() {
            let index = line * snapshot.grid.columns() + column;
            let cell = &snapshot.cells[index];
            let is_selected = selected(column as u16, line as u16);
            let (mut foreground, mut background) =
                resolve_cell_colors(cell.foreground, cell.background, colors);
            if cell.flags.inverse {
                std::mem::swap(&mut foreground, &mut background);
            }
            if cell.flags.dim {
                foreground = dim(foreground);
            }
            if is_selected {
                foreground = colors.selection_foreground.0;
                background = colors.selection_background.0;
            }
            if background_color != Some(background) {
                if let Some(previous) = background_color {
                    rectangles.push(cell_rectangle(
                        layout,
                        background_start,
                        line,
                        column - background_start,
                        previous,
                    ));
                }
                background_start = column;
                background_color = Some(background);
            }
            if !matches!(cell.width, CellWidth::Spacer | CellWidth::LeadingSpacer)
                && !cell.flags.hidden
                && (cell.ch != ' ' || cell.zerowidth.is_some())
            {
                let mut cluster = String::from(cell.ch);
                if let Some(zero) = &cell.zerowidth {
                    cluster.extend(zero.iter());
                }
                let style = match (cell.flags.bold, cell.flags.italic) {
                    (true, true) => FontStyle::BoldItalic,
                    (true, false) => FontStyle::Bold,
                    (false, true) => FontStyle::Italic,
                    (false, false) => FontStyle::Regular,
                };
                let shaped = text.shape_cluster(&cluster, style)?;
                let span = if cell.width == CellWidth::Wide { 2 } else { 1 };
                let origin = cell_origin(layout, column, line);
                let mut pen_x = 0_i32;
                for glyph in shaped.glyphs {
                    let bitmap = shaped
                        .assets
                        .iter()
                        .find(|asset| asset.key == glyph.key)
                        .ok_or(ComposeError::Snapshot("shaper omitted glyph asset"))?;
                    if bitmap.bitmap.size_px != [0, 0] {
                        glyphs.push(GlyphPlacement {
                            key: glyph.key,
                            origin_px: [
                                origin[0] as i32
                                    + i32::from(bitmap.bitmap.bearing_px[0])
                                    + (pen_x + glyph.offset_26_6[0]) / 64,
                                origin[1] as i32 + i32::from(metrics.baseline_px)
                                    - i32::from(bitmap.bitmap.bearing_px[1])
                                    - glyph.offset_26_6[1] / 64,
                            ],
                            clip_px: [
                                origin[0],
                                origin[1],
                                u32::from(layout.cell_px[0].get()) * span,
                                u32::from(layout.cell_px[1].get()),
                            ],
                            color: LinearColor::from_srgba8(foreground),
                        });
                    }
                    pen_x = pen_x.saturating_add(glyph.advance_26_6[0]);
                }
                for asset in shaped.assets {
                    assets.entry(asset.key).or_insert(asset);
                }
                if assets.len() > MAX_UNIQUE_GLYPHS {
                    return Err(ComposeError::Capacity("unique glyphs"));
                }
            }
            if cell.flags.underline || cell.flags.strikeout {
                let origin = cell_origin(layout, column, line);
                let span = if cell.width == CellWidth::Wide { 2 } else { 1 };
                if cell.flags.underline {
                    rectangles.push(line_rectangle(
                        origin,
                        layout,
                        span,
                        metrics.underline_y_px,
                        metrics.underline_thickness_px.get(),
                        foreground,
                    ));
                }
                if cell.flags.strikeout {
                    rectangles.push(line_rectangle(
                        origin,
                        layout,
                        span,
                        metrics.strike_y_px,
                        metrics.strike_thickness_px.get(),
                        foreground,
                    ));
                }
            }
        }
        if let Some(color) = background_color {
            rectangles.push(cell_rectangle(
                layout,
                background_start,
                line,
                snapshot.grid.columns() - background_start,
                color,
            ));
        }
    }
    if snapshot.cursor.visible
        && usize::from(snapshot.cursor.column) < snapshot.grid.columns()
        && usize::from(snapshot.cursor.line) < snapshot.grid.lines()
    {
        let origin = cell_origin(
            layout,
            usize::from(snapshot.cursor.column),
            usize::from(snapshot.cursor.line),
        );
        let (cursor_origin, cursor_size) = match cursor_style {
            CursorStyle::Block => (
                origin,
                [
                    u32::from(layout.cell_px[0].get()),
                    u32::from(layout.cell_px[1].get()),
                ],
            ),
            CursorStyle::Beam => (origin, [1, u32::from(layout.cell_px[1].get())]),
            CursorStyle::Underline => (
                [
                    origin[0],
                    origin[1] + u32::from(layout.cell_px[1].get()) - 1,
                ],
                [u32::from(layout.cell_px[0].get()), 1],
            ),
        };
        rectangles.push(RectangleInstance {
            origin_px: [cursor_origin[0] as f32, cursor_origin[1] as f32],
            size_px: [cursor_size[0] as f32, cursor_size[1] as f32],
            color: LinearColor::from_srgba8(colors.cursor.0),
        });
    }
    Ok(SceneData {
        clear: LinearColor::from_srgba8(colors.background.0),
        rectangles,
        glyphs,
        glyph_assets: assets.into_values().collect(),
        source_generation: snapshot.generation,
        font_generation: layout.font_generation,
    })
}

fn validate_widths(snapshot: &FrameSnapshot) -> Result<(), ComposeError> {
    for line in 0..snapshot.grid.lines() {
        let row =
            &snapshot.cells[line * snapshot.grid.columns()..(line + 1) * snapshot.grid.columns()];
        for (column, cell) in row.iter().enumerate() {
            match cell.width {
                CellWidth::Wide
                    if row
                        .get(column + 1)
                        .is_none_or(|next| next.width != CellWidth::Spacer) =>
                {
                    return Err(ComposeError::Snapshot(
                        "wide cell is not followed by spacer",
                    ));
                }
                CellWidth::Spacer if column == 0 || row[column - 1].width != CellWidth::Wide => {
                    return Err(ComposeError::Snapshot("orphan spacer cell"));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn point_in_range(point: [u16; 2], range: CellRange) -> bool {
    let point = (point[1], point[0]);
    let start = (range.start[1], range.start[0]);
    let end = (range.end[1], range.end[0]);
    point >= start.min(end) && point <= start.max(end)
}

fn resolve_cell_colors(
    foreground: TerminalColor,
    background: TerminalColor,
    colors: &ColorsConfig,
) -> (u32, u32) {
    (
        resolve_color(foreground, colors.foreground.0, colors),
        resolve_color(background, colors.background.0, colors),
    )
}

fn resolve_color(color: TerminalColor, default: u32, colors: &ColorsConfig) -> u32 {
    match color {
        TerminalColor::Rgb(r, g, b) => u32::from_be_bytes([r, g, b, 255]),
        TerminalColor::Indexed(index) => indexed(index),
        TerminalColor::Named(256) => colors.foreground.0,
        TerminalColor::Named(257) => colors.background.0,
        TerminalColor::Named(258) => colors.cursor.0,
        TerminalColor::Named(index @ 0..=15) => indexed(u8::try_from(index).unwrap_or(0)),
        TerminalColor::Named(_) => default,
    }
}

fn indexed(index: u8) -> u32 {
    #[allow(clippy::unreadable_literal)]
    const ANSI: [u32; 16] = [
        0x000000ff, 0xcd0000ff, 0x00cd00ff, 0xcdcd00ff, 0x0000eeff, 0xcd00cdff, 0x00cdcdff,
        0xe5e5e5ff, 0x7f7f7fff, 0xff0000ff, 0x00ff00ff, 0xffff00ff, 0x5c5cffff, 0xff00ffff,
        0x00ffffff, 0xffffffff,
    ];
    if index < 16 {
        return ANSI[index as usize];
    }
    if index < 232 {
        let value = index - 16;
        let component = |part: u8| if part == 0 { 0 } else { 55 + part * 40 };
        return u32::from_be_bytes([
            component(value / 36),
            component(value / 6 % 6),
            component(value % 6),
            255,
        ]);
    }
    let gray = 8 + (index - 232) * 10;
    u32::from_be_bytes([gray, gray, gray, 255])
}

fn dim(color: u32) -> u32 {
    let [r, g, b, a] = color.to_be_bytes();
    u32::from_be_bytes([r / 2, g / 2, b / 2, a])
}
#[allow(clippy::cast_possible_truncation)]
fn cell_origin(layout: &GridLayout, column: usize, line: usize) -> [u32; 2] {
    [
        layout.content_origin_px[0] + column as u32 * u32::from(layout.cell_px[0].get()),
        layout.content_origin_px[1] + line as u32 * u32::from(layout.cell_px[1].get()),
    ]
}
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn cell_rectangle(
    layout: &GridLayout,
    start: usize,
    line: usize,
    count: usize,
    color: u32,
) -> RectangleInstance {
    let origin = cell_origin(layout, start, line);
    RectangleInstance {
        origin_px: [origin[0] as f32, origin[1] as f32],
        size_px: [
            (count as u32 * u32::from(layout.cell_px[0].get())) as f32,
            f32::from(layout.cell_px[1].get()),
        ],
        color: LinearColor::from_srgba8(color),
    }
}
#[allow(clippy::cast_precision_loss)]
fn line_rectangle(
    origin: [u32; 2],
    layout: &GridLayout,
    span: u32,
    y: i16,
    thickness: u16,
    color: u32,
) -> RectangleInstance {
    RectangleInstance {
        origin_px: [
            origin[0] as f32,
            (i64::from(origin[1]) + i64::from(y)).max(0) as f32,
        ],
        size_px: [
            (u32::from(layout.cell_px[0].get()) * span) as f32,
            f32::from(thickness),
        ],
        color: LinearColor::from_srgba8(color),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ComposeError {
    #[error(transparent)]
    Text(#[from] TextError),
    #[error("snapshot contract error: {0}")]
    Snapshot(&'static str),
    #[error("frame exceeds hard capacity: {0}")]
    Capacity(&'static str),
}

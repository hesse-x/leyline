use std::{collections::HashMap, sync::Arc};

use leyline_gfx::{GlyphPlacement, LinearColor, RectangleInstance, SceneData};
use leyline_text::{FontStyle, GlyphAsset, GlyphKey, ShapedCluster, TextError, TextSystem};

use crate::{
    clipboard::{PasteConfirmationOverlay, PasteRisk, TransferTarget},
    config::{ColorsConfig, CursorStyle},
    interaction::PreeditOverlay,
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

#[derive(Clone, Copy, Debug)]
pub struct FrameOverlays<'a> {
    pub selection: &'a SelectionOverlay,
    pub preedit: Option<&'a PreeditOverlay>,
    pub paste_confirmation: Option<&'a PasteConfirmationOverlay>,
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
    overlays: FrameOverlays<'_>,
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
        overlays.selection.snapshot_generation == snapshot.generation
            && overlays
                .selection
                .ranges
                .iter()
                .any(|range| point_in_range([column, line], *range))
    };
    let mut rectangles = Vec::new();
    let mut glyphs = Vec::new();
    let mut assets = HashMap::<GlyphKey, GlyphAsset>::new();
    let mut shaped_cache = HashMap::<(String, FontStyle), ShapedCluster>::new();
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
            // The clear color already supplies the default glass background. Only paint cells
            // that intentionally differ, otherwise alpha would accumulate and look opaque.
            let painted_background =
                (cell.background != TerminalColor::Named(257) || cell.flags.inverse || is_selected)
                    .then_some(background);
            if background_color != painted_background {
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
                background_color = painted_background;
            }
            let glyph_color = cursor_glyph_color(
                foreground,
                background,
                cursor_style,
                snapshot.cursor.visible
                    && usize::from(snapshot.cursor.column) == column
                    && usize::from(snapshot.cursor.line) == line,
            );
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
                let shaped = if let Some(shaped) = shaped_cache.get(&(cluster.clone(), style)) {
                    shaped.clone()
                } else {
                    let shaped = text.shape_cluster(&cluster, style)?;
                    shaped_cache.insert((cluster, style), shaped.clone());
                    shaped
                };
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
                            color: LinearColor::from_srgba8(glyph_color),
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
    if let Some(preedit) = overlays
        .preedit
        .filter(|value| value.snapshot_generation == snapshot.generation)
        && usize::from(preedit.anchor[0]) < snapshot.grid.columns()
        && usize::from(preedit.anchor[1]) < snapshot.grid.lines()
        && !preedit.text.is_empty()
    {
        let origin = cell_origin(
            layout,
            usize::from(preedit.anchor[0]),
            usize::from(preedit.anchor[1]),
        );
        let shaped = text.shape_cluster(&preedit.text, FontStyle::Regular)?;
        let mut pen_x = 0_i32;
        for glyph in shaped.glyphs {
            let bitmap = shaped
                .assets
                .iter()
                .find(|asset| asset.key == glyph.key)
                .ok_or(ComposeError::Snapshot("shaper omitted preedit glyph asset"))?;
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
                        layout.viewport_px.width.saturating_sub(origin[0]),
                        u32::from(layout.cell_px[1].get()),
                    ],
                    color: LinearColor::from_srgba8(colors.foreground.0),
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
        let underline_width = u32::try_from((pen_x.max(64) + 63) / 64).unwrap_or(u32::MAX);
        rectangles.push(RectangleInstance {
            origin_px: [
                origin[0] as f32,
                (origin[1] + u32::from(layout.cell_px[1].get()) - 1) as f32,
            ],
            size_px: [underline_width as f32, 1.0],
            color: LinearColor::from_srgba8(colors.foreground.0),
        });
    }
    if let Some(confirmation) = overlays.paste_confirmation {
        compose_paste_confirmation(
            text,
            confirmation,
            layout,
            &mut rectangles,
            &mut glyphs,
            &mut assets,
        )?;
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

#[allow(clippy::cast_precision_loss)]
fn compose_paste_confirmation(
    text: &mut TextSystem,
    confirmation: &PasteConfirmationOverlay,
    layout: &GridLayout,
    rectangles: &mut Vec<RectangleInstance>,
    glyphs: &mut Vec<GlyphPlacement>,
    assets: &mut HashMap<GlyphKey, GlyphAsset>,
) -> Result<(), ComposeError> {
    let source = match confirmation.source {
        TransferTarget::Clipboard => "Clipboard",
        TransferTarget::Primary => "Primary selection",
    };
    let risk = match confirmation.risk {
        PasteRisk::Multiline => "Multiple lines may execute commands",
        PasteRisk::ControlCharacters => "Control characters may alter terminal state",
    };
    let lines = [
        "PASTE REQUIRES CONFIRMATION".to_owned(),
        format!(
            "Source: {source}   {} bytes   {} lines",
            confirmation.bytes, confirmation.lines
        ),
        format!("Risk: {risk}"),
        "Enter / Y  Paste     Esc / N  Cancel".to_owned(),
    ];
    let cell_width = u32::from(layout.cell_px[0].get());
    let cell_height = u32::from(layout.cell_px[1].get());
    let margin = cell_width.saturating_mul(2);
    let panel_width = cell_width
        .saturating_mul(62)
        .min(layout.viewport_px.width.saturating_sub(margin).max(1));
    let panel_height = cell_height
        .saturating_mul(6)
        .min(layout.viewport_px.height.max(1));
    let panel_origin = [
        layout.viewport_px.width.saturating_sub(panel_width) / 2,
        layout.viewport_px.height.saturating_sub(panel_height) / 2,
    ];

    // The modal card deliberately hides terminal content and never receives clipboard text.
    glyphs.clear();
    assets.clear();
    rectangles.push(RectangleInstance {
        origin_px: [0.0, 0.0],
        size_px: [
            layout.viewport_px.width as f32,
            layout.viewport_px.height as f32,
        ],
        color: LinearColor::from_srgba8(0x090b_0fff),
    });
    rectangles.push(RectangleInstance {
        origin_px: [panel_origin[0] as f32, panel_origin[1] as f32],
        size_px: [panel_width as f32, panel_height as f32],
        color: LinearColor::from_srgba8(0x1c22_29ff),
    });
    rectangles.push(RectangleInstance {
        origin_px: [panel_origin[0] as f32, panel_origin[1] as f32],
        size_px: [panel_width as f32, 3.0],
        color: LinearColor::from_srgba8(0xf2b8_4bff),
    });

    let text_x = panel_origin[0].saturating_add(cell_width.saturating_mul(2));
    for (index, line) in lines.iter().enumerate() {
        let line_index = u32::try_from(index).unwrap_or(u32::MAX);
        let origin = [
            text_x,
            panel_origin[1].saturating_add(cell_height.saturating_mul(line_index + 1)),
        ];
        let color = if index == 0 { 0xf2b8_4bff } else { 0xf4f1_e8ff };
        append_overlay_text(
            text,
            line,
            origin,
            panel_origin,
            [panel_width, panel_height],
            color,
            glyphs,
            assets,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::cast_possible_wrap)]
fn append_overlay_text(
    text: &mut TextSystem,
    value: &str,
    origin: [u32; 2],
    panel_origin: [u32; 2],
    panel_size: [u32; 2],
    color: u32,
    glyphs: &mut Vec<GlyphPlacement>,
    assets: &mut HashMap<GlyphKey, GlyphAsset>,
) -> Result<(), ComposeError> {
    let shaped = text.shape_cluster(value, FontStyle::Regular)?;
    let metrics = text.metrics();
    let mut pen_x = 0_i32;
    for glyph in shaped.glyphs {
        let bitmap = shaped
            .assets
            .iter()
            .find(|asset| asset.key == glyph.key)
            .ok_or(ComposeError::Snapshot("shaper omitted overlay glyph asset"))?;
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
                    panel_origin[0],
                    panel_origin[1],
                    panel_size[0],
                    panel_size[1],
                ],
                color: LinearColor::from_srgba8(color),
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
    Ok(())
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

const fn cursor_glyph_color(
    foreground: u32,
    background: u32,
    cursor_style: CursorStyle,
    is_cursor_cell: bool,
) -> u32 {
    if is_cursor_cell && matches!(cursor_style, CursorStyle::Block) {
        background
    } else {
        foreground
    }
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

#[cfg(test)]
mod tests {
    use super::cursor_glyph_color;
    use crate::config::CursorStyle;

    #[test]
    fn block_cursor_preserves_the_character_with_contrasting_color() {
        let foreground = 0xffff_ffff;
        let background = 0x1010_10ff;
        assert_eq!(
            cursor_glyph_color(foreground, background, CursorStyle::Block, true),
            background
        );
        assert_eq!(
            cursor_glyph_color(foreground, background, CursorStyle::Beam, true),
            foreground
        );
        assert_eq!(
            cursor_glyph_color(foreground, background, CursorStyle::Block, false),
            foreground
        );
    }
}

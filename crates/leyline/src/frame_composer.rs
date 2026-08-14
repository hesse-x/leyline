use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use leyline_gfx::{GlyphPlacement, LinearColor, RectangleInstance, SceneData};
use leyline_text::{
    CellMetrics, FontStyle, GlyphAsset, GlyphKey, MAX_GLYPH_BITMAP_BYTES, MAX_GLYPH_BITMAPS,
    MAX_PREPARED_GLYPHS, ShapedRun, TextError, TextSystem,
};

use crate::{
    clipboard::{PasteConfirmationOverlay, PasteRisk, TransferTarget},
    config::{ColorsConfig, CursorStyle},
    interaction::{PreeditOverlay, ScrollbarPresentation},
    layout::GridLayout,
    terminal::{CellWidth, FrameSnapshot, TerminalColor},
};

pub const MAX_UNIQUE_GLYPHS: usize = MAX_GLYPH_BITMAPS;

type ShapedCache = HashMap<(String, FontStyle), ShapedRun>;
type GlyphAssets = HashMap<GlyphKey, GlyphAsset>;

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
    pub scrollbar: Option<&'a ScrollbarPresentation>,
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
    text.begin_scene();
    let result = compose_active_scene(text, snapshot, overlays, layout, colors, cursor_style);
    text.end_scene();
    result
}

#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]
fn compose_active_scene(
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
    let (shaped_cache, mut assets) = prepare_glyph_working_set(
        text,
        snapshot,
        overlays.paste_confirmation,
        overlays.preedit,
    )?;
    let mut rectangles = Vec::new();
    let mut glyphs = Vec::new();
    let metrics = layout.cell_metrics;
    if let Some(confirmation) = overlays.paste_confirmation {
        compose_paste_confirmation(
            confirmation,
            layout,
            metrics,
            &shaped_cache,
            &mut rectangles,
            &mut glyphs,
            &mut assets,
        )?;
        validate_glyph_working_set(&glyphs, &assets)?;
        return Ok(SceneData {
            clear: LinearColor::from_srgba8(colors.background.0),
            rectangles,
            glyphs,
            glyph_assets: assets.into_values().collect(),
            source_generation: snapshot.generation,
            font_generation: layout.font_generation,
        });
    }
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
                let shaped = shaped_cache
                    .get(&(cluster, style))
                    .ok_or(ComposeError::Snapshot("missing shaped cell cluster"))?;
                let span = if cell.width == CellWidth::Wide { 2 } else { 1 };
                let origin = cell_origin(layout, column, line);
                let mut pen_x = 0_i32;
                for glyph in &shaped.glyphs {
                    let bitmap = assets
                        .get(&glyph.key)
                        .ok_or(ComposeError::Snapshot("rasterizer omitted glyph asset"))?;
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
            }
            if cell.flags.underline || cell.flags.strikeout {
                let origin = cell_origin(layout, column, line);
                let span = if cell.width == CellWidth::Wide { 2 } else { 1 };
                if cell.flags.underline {
                    let underline = cell
                        .underline_color
                        .map_or(foreground, |color| resolve_color(color, foreground, colors));
                    rectangles.push(line_rectangle(
                        origin,
                        layout,
                        span,
                        metrics.underline_y_px,
                        metrics.underline_thickness_px.get(),
                        underline,
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
        let shaped = shaped_cache
            .get(&(preedit.text.to_string(), FontStyle::Regular))
            .ok_or(ComposeError::Snapshot("missing shaped preedit cluster"))?;
        let mut pen_x = 0_i32;
        for glyph in &shaped.glyphs {
            let bitmap = assets.get(&glyph.key).ok_or(ComposeError::Snapshot(
                "rasterizer omitted preedit glyph asset",
            ))?;
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
    if let Some(scrollbar) = overlays.scrollbar {
        for (rect, color) in [
            (scrollbar.track, scrollbar.track_color),
            (scrollbar.thumb, scrollbar.thumb_color),
        ] {
            if rect.width > 0.0 && rect.height > 0.0 && color.0.to_be_bytes()[3] != 0 {
                rectangles.push(RectangleInstance {
                    origin_px: [rect.x as f32, rect.y as f32],
                    size_px: [rect.width as f32, rect.height as f32],
                    color: LinearColor::from_srgba8(color.0),
                });
            }
        }
    }
    validate_glyph_working_set(&glyphs, &assets)?;
    Ok(SceneData {
        clear: LinearColor::from_srgba8(colors.background.0),
        rectangles,
        glyphs,
        glyph_assets: assets.into_values().collect(),
        source_generation: snapshot.generation,
        font_generation: layout.font_generation,
    })
}

fn prepare_glyph_working_set(
    text: &mut TextSystem,
    snapshot: &FrameSnapshot,
    paste_confirmation: Option<&PasteConfirmationOverlay>,
    preedit: Option<&PreeditOverlay>,
) -> Result<(ShapedCache, GlyphAssets), ComposeError> {
    let mut request_set = HashSet::<(String, FontStyle)>::new();
    let mut requests = Vec::<(String, FontStyle)>::new();
    let mut request = |cluster: String, style: FontStyle| {
        if request_set.insert((cluster.clone(), style)) {
            requests.push((cluster, style));
        }
    };
    if let Some(confirmation) = paste_confirmation {
        for line in paste_confirmation_lines(confirmation) {
            request(line, FontStyle::Regular);
        }
    } else {
        for cell in snapshot.cells.iter() {
            if matches!(cell.width, CellWidth::Spacer | CellWidth::LeadingSpacer)
                || cell.flags.hidden
                || (cell.ch == ' ' && cell.zerowidth.is_none())
            {
                continue;
            }
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
            request(cluster, style);
        }
        if let Some(preedit) = preedit
            && preedit.snapshot_generation == snapshot.generation
            && !preedit.text.is_empty()
        {
            request(preedit.text.to_string(), FontStyle::Regular);
        }
    }

    let mut shaped = HashMap::with_capacity(requests.len());
    let mut key_set = HashSet::new();
    let mut keys = Vec::new();
    for (cluster, style) in requests {
        let run = text.shape_cluster_only(&cluster, style)?;
        for key in run.glyphs.iter().map(|glyph| glyph.key) {
            if key_set.insert(key) {
                keys.push(key);
            }
        }
        if keys.len() > MAX_UNIQUE_GLYPHS {
            return Err(ComposeError::Capacity("unique glyphs"));
        }
        shaped.insert((cluster, style), run);
    }
    let placements = count_prepared_placements(snapshot, paste_confirmation, preedit, &shaped);
    if placements.is_none_or(|count| count > MAX_PREPARED_GLYPHS) {
        return Err(ComposeError::Capacity("glyph placements"));
    }
    let assets = text
        .rasterize_working_set(&keys)?
        .into_iter()
        .map(|asset| (asset.key, asset))
        .collect();
    Ok((shaped, assets))
}

fn count_prepared_placements(
    snapshot: &FrameSnapshot,
    paste_confirmation: Option<&PasteConfirmationOverlay>,
    preedit: Option<&PreeditOverlay>,
    shaped: &ShapedCache,
) -> Option<usize> {
    if let Some(confirmation) = paste_confirmation {
        paste_confirmation_lines(confirmation)
            .iter()
            .try_fold(0_usize, |total, line| {
                let count = shaped
                    .get(&(line.clone(), FontStyle::Regular))
                    .map_or(0, |run| run.glyphs.len());
                total.checked_add(count)
            })
    } else {
        let mut total = 0_usize;
        for cell in snapshot.cells.iter().filter(|cell| {
            !matches!(cell.width, CellWidth::Spacer | CellWidth::LeadingSpacer)
                && !cell.flags.hidden
                && (cell.ch != ' ' || cell.zerowidth.is_some())
        }) {
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
            total = total.checked_add(
                shaped
                    .get(&(cluster, style))
                    .map_or(0, |run| run.glyphs.len()),
            )?;
        }
        if let Some(preedit) = preedit
            && preedit.snapshot_generation == snapshot.generation
            && usize::from(preedit.anchor[0]) < snapshot.grid.columns()
            && usize::from(preedit.anchor[1]) < snapshot.grid.lines()
            && !preedit.text.is_empty()
        {
            let count = shaped
                .get(&(preedit.text.to_string(), FontStyle::Regular))
                .map_or(0, |run| run.glyphs.len());
            total = total.checked_add(count)?;
        }
        Some(total)
    }
}

fn validate_glyph_working_set(
    glyphs: &[GlyphPlacement],
    assets: &GlyphAssets,
) -> Result<(), ComposeError> {
    let bitmap_bytes = assets.values().try_fold(0_usize, |total, asset| {
        total.checked_add(asset.bitmap.coverage.len())
    });
    validate_glyph_budget(glyphs.len(), assets.len(), bitmap_bytes)
}

fn validate_glyph_budget(
    placements: usize,
    unique_bitmaps: usize,
    bitmap_bytes: Option<usize>,
) -> Result<(), ComposeError> {
    if placements > MAX_PREPARED_GLYPHS {
        return Err(ComposeError::Capacity("glyph placements"));
    }
    if unique_bitmaps > MAX_UNIQUE_GLYPHS {
        return Err(ComposeError::Capacity("unique glyphs"));
    }
    if bitmap_bytes.is_none_or(|bytes| bytes > MAX_GLYPH_BITMAP_BYTES) {
        return Err(ComposeError::Capacity("glyph bitmap bytes"));
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn compose_paste_confirmation(
    confirmation: &PasteConfirmationOverlay,
    layout: &GridLayout,
    metrics: CellMetrics,
    shaped_cache: &ShapedCache,
    rectangles: &mut Vec<RectangleInstance>,
    glyphs: &mut Vec<GlyphPlacement>,
    assets: &mut GlyphAssets,
) -> Result<(), ComposeError> {
    let lines = paste_confirmation_lines(confirmation);
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
            line,
            origin,
            panel_origin,
            [panel_width, panel_height],
            color,
            metrics,
            shaped_cache,
            glyphs,
            assets,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::cast_possible_wrap)]
fn append_overlay_text(
    value: &str,
    origin: [u32; 2],
    panel_origin: [u32; 2],
    panel_size: [u32; 2],
    color: u32,
    metrics: CellMetrics,
    shaped_cache: &ShapedCache,
    glyphs: &mut Vec<GlyphPlacement>,
    assets: &mut GlyphAssets,
) -> Result<(), ComposeError> {
    let shaped = shaped_cache
        .get(&(value.to_owned(), FontStyle::Regular))
        .ok_or(ComposeError::Snapshot("missing shaped overlay cluster"))?;
    let mut pen_x = 0_i32;
    for glyph in &shaped.glyphs {
        let bitmap = assets.get(&glyph.key).ok_or(ComposeError::Snapshot(
            "rasterizer omitted overlay glyph asset",
        ))?;
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
    Ok(())
}

fn paste_confirmation_lines(confirmation: &PasteConfirmationOverlay) -> [String; 4] {
    let source = match confirmation.source {
        TransferTarget::Clipboard => "Clipboard",
        TransferTarget::Primary => "Primary selection",
    };
    let risk = match confirmation.risk {
        PasteRisk::Multiline => "Multiple lines may execute commands",
        PasteRisk::ControlCharacters => "Control characters may alter terminal state",
    };
    [
        "PASTE REQUIRES CONFIRMATION".to_owned(),
        format!(
            "Source: {source}   {} bytes   {} lines",
            confirmation.bytes, confirmation.lines
        ),
        format!("Risk: {risk}"),
        "Enter / Y  Paste     Esc / N  Cancel".to_owned(),
    ]
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
        TerminalColor::Indexed(index) => indexed(index, colors),
        TerminalColor::Named(256) => colors.foreground.0,
        TerminalColor::Named(257) => colors.background.0,
        TerminalColor::Named(258) => colors.cursor.0,
        TerminalColor::Named(index @ 0..=15) => colors.ansi[usize::from(index)].0,
        TerminalColor::Named(_) => default,
    }
}

fn indexed(index: u8, colors: &ColorsConfig) -> u32 {
    if index < 16 {
        return colors.ansi[index as usize].0;
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
    let dim = |byte| {
        let srgb = f64::from(byte) / 255.0;
        let linear = if srgb <= 0.040_45 {
            srgb / 12.92
        } else {
            ((srgb + 0.055) / 1.055).powf(2.4)
        } * 0.5;
        let srgb = if linear <= 0.003_130_8 {
            linear * 12.92
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            (srgb * 255.0).round() as u8
        }
    };
    u32::from_be_bytes([dim(r), dim(g), dim(b), a])
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
    use super::{cursor_glyph_color, dim, resolve_color, validate_glyph_budget};
    use crate::config::CursorStyle;
    use leyline_text::{MAX_GLYPH_BITMAP_BYTES, MAX_GLYPH_BITMAPS, MAX_PREPARED_GLYPHS};

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

    #[test]
    fn glyph_working_set_limits_are_independent_and_inclusive() {
        assert!(
            validate_glyph_budget(
                MAX_PREPARED_GLYPHS,
                MAX_GLYPH_BITMAPS,
                Some(MAX_GLYPH_BITMAP_BYTES),
            )
            .is_ok()
        );
        assert!(validate_glyph_budget(MAX_PREPARED_GLYPHS + 1, 0, Some(0)).is_err());
        assert!(validate_glyph_budget(0, MAX_GLYPH_BITMAPS + 1, Some(0)).is_err());
        assert!(validate_glyph_budget(0, 0, Some(MAX_GLYPH_BITMAP_BYTES + 1)).is_err());
        assert!(validate_glyph_budget(0, 0, None).is_err());
    }

    #[test]
    fn named_and_low_index_colors_use_the_configured_palette() {
        let mut colors = crate::config::EffectiveConfig::default().colors;
        colors.ansi[3] = crate::config::Color(0x1234_56ff);
        assert_eq!(
            resolve_color(crate::terminal::TerminalColor::Named(3), 0, &colors),
            0x1234_56ff
        );
        assert_eq!(
            resolve_color(crate::terminal::TerminalColor::Indexed(3), 0, &colors),
            0x1234_56ff
        );
        assert_eq!(
            resolve_color(crate::terminal::TerminalColor::Rgb(1, 2, 3), 0, &colors),
            0x0102_03ff
        );
    }

    #[test]
    fn dim_halves_linear_light_instead_of_srgb_bytes() {
        let [value, _, _, alpha] = dim(0x8080_80ff).to_be_bytes();
        assert_eq!(alpha, 255);
        assert!(
            value > 64,
            "linear-light dimming must remain brighter than byte halving"
        );
    }
}

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use leyline_gfx::{GlyphPlacement, LinearColor, RectangleInstance, SceneData};
use leyline_text::{
    CellMetrics, FontStyle, GlyphAsset, GlyphKey, MAX_GLYPH_BITMAP_BYTES, MAX_GLYPH_BITMAPS,
    MAX_PREPARED_GLYPHS, ShapedRun, TextDirection, TextError, TextSystem,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    clipboard::{PasteConfirmationOverlay, PasteRisk, TransferTarget},
    config::{ColorsConfig, CursorStyle},
    interaction::{PreeditOverlay, ScrollbarPresentation},
    layout::GridLayout,
    terminal::{
        CellWidth, CursorBlink, CursorShape, FrameSnapshot, SnapshotCell, TerminalColor,
        UnderlineStyle,
    },
    unicode_layout::VisualGridMap,
};

pub const MAX_UNIQUE_GLYPHS: usize = MAX_GLYPH_BITMAPS;
pub const MAX_DECORATION_PRIMITIVES: usize = 262_144;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorPresentationPolicy {
    pub blink_phase_visible: bool,
}

impl From<CursorStyle> for CursorPresentationPolicy {
    fn from(_: CursorStyle) -> Self {
        Self {
            blink_phase_visible: true,
        }
    }
}

const TAB_BAR_BACKGROUND: u32 = 0x2b2b_2bff;
const TAB_ACTIVE_BACKGROUND: u32 = 0x3333_33ff;
const TAB_DIVIDER: u32 = 0x4141_41ff;
const TAB_ACCENT: u32 = 0xff5a_36ff;
const TAB_ACTIVE_TEXT: u32 = 0xeded_edff;
const TAB_INACTIVE_TEXT: u32 = 0xa8a8_a8ff;
const TAB_UNREAD_TEXT: u32 = 0xd8d8_d8ff;
const TAB_CLOSE_MARK: &str = "\u{00d7}";
const TAB_FONT_KEY_NAMESPACE: u64 = 1 << 63;

type ShapedCache = HashMap<(String, FontStyle), ShapedRun>;
struct TerminalShape {
    run: ShapedRun,
    span: u8,
}
type TerminalShapes = HashMap<(usize, usize), TerminalShape>;
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
    pub tab_bar: Option<&'a crate::tab::TabBarPresentation>,
}

/// Converts one validated terminal snapshot into a bounded graphics scene.
///
/// # Errors
/// Returns a typed error for invalid snapshot contracts, font failures, or capacity limits.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]
pub fn compose(
    text: &mut TextSystem,
    tab_text: &mut TextSystem,
    snapshot: &FrameSnapshot,
    overlays: FrameOverlays<'_>,
    layout: &GridLayout,
    colors: &ColorsConfig,
    cursor_policy: impl Into<CursorPresentationPolicy>,
    visual_map: &VisualGridMap,
    layout_generation: u64,
) -> Result<SceneData, ComposeError> {
    text.begin_scene();
    tab_text.begin_scene();
    let result = compose_active_scene(
        text,
        tab_text,
        snapshot,
        overlays,
        layout,
        colors,
        cursor_policy.into(),
        visual_map,
        layout_generation,
    );
    text.end_scene();
    tab_text.end_scene();
    result
}

/// Rebuilds a prepared scene's color working set as grayscale coverage.
///
/// This is the sole capacity fallback and intentionally preserves glyph keys and placements.
pub(crate) fn downgrade_color_working_set(scene: &mut SceneData) -> bool {
    let mut changed = false;
    for asset in &mut scene.glyph_assets {
        if asset.bitmap.format != leyline_text::GlyphFormat::ColorSrgba8 {
            continue;
        }
        let coverage = asset
            .bitmap
            .pixels
            .chunks_exact(4)
            .map(|pixel| pixel[3])
            .collect::<Vec<_>>();
        asset.bitmap.format = leyline_text::GlyphFormat::Gray8;
        asset.bitmap.pixels = Arc::from(coverage);
        changed = true;
    }
    changed
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]
fn compose_active_scene(
    text: &mut TextSystem,
    tab_text: &mut TextSystem,
    snapshot: &FrameSnapshot,
    overlays: FrameOverlays<'_>,
    layout: &GridLayout,
    colors: &ColorsConfig,
    cursor_policy: CursorPresentationPolicy,
    visual_map: &VisualGridMap,
    layout_generation: u64,
) -> Result<SceneData, ComposeError> {
    if snapshot.grid != layout.grid
        || snapshot.cells.len() != snapshot.grid.columns() * snapshot.grid.lines()
        || visual_map.grid != snapshot.grid
        || visual_map.snapshot_generation != snapshot.generation
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
    let (shaped_cache, terminal_shapes, mut assets) = prepare_glyph_working_set(
        text,
        snapshot,
        overlays.paste_confirmation,
        overlays.preedit,
        visual_map,
    )?;
    let (tab_shaped_cache, tab_assets) =
        match prepare_tab_glyph_working_set(tab_text, overlays.tab_bar) {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::warn!(category = "tab_text", %error, "tab text omitted for this frame");
                (ShapedCache::new(), GlyphAssets::new())
            }
        };
    assets.extend(tab_assets);
    let tab_metrics = tab_text.metrics();
    let mut rectangles = Vec::new();
    let mut glyphs = Vec::new();
    let metrics = layout.cell_metrics;
    let cursor_style = match snapshot.cursor.shape {
        CursorShape::Block => CursorStyle::Block,
        CursorShape::Beam => CursorStyle::Beam,
        CursorShape::Underline => CursorStyle::Underline,
    };
    let cursor_visible = snapshot.cursor.visible
        && (snapshot.cursor.blink == CursorBlink::Steady || cursor_policy.blink_phase_visible);
    let mut decoration_primitives = 0_usize;
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
            glyph_assets: sorted_assets(assets),
            source_generation: snapshot.generation,
            font_generation: layout.font_generation,
            frame_key: leyline_gfx::FrameKey {
                snapshot_generation: snapshot.generation,
                layout_generation,
                font_generation: layout.font_generation,
                unicode_policy_generation: visual_map.policy_generation,
            },
        });
    }
    for line in 0..snapshot.grid.lines() {
        for column in 0..snapshot.grid.columns() {
            let visual_column = usize::from(visual_map.lines[line].logical_to_visual_cell[column]);
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
            if let Some(background) = painted_background {
                rectangles.push(cell_rectangle(layout, visual_column, line, 1, background));
            }
            let glyph_color = cursor_glyph_color(
                foreground,
                background,
                cursor_style,
                cursor_visible
                    && usize::from(snapshot.cursor.column) == column
                    && usize::from(snapshot.cursor.line) == line,
            );
            if !matches!(cell.width, CellWidth::Spacer | CellWidth::LeadingSpacer)
                && !cell.flags.hidden
                && !is_bidi_control(cell.ch)
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
                let shaped = terminal_shapes
                    .get(&(line, column))
                    .map(|shape| &shape.run)
                    .or_else(|| shaped_cache.get(&(cluster, style)))
                    .ok_or(ComposeError::Snapshot("missing shaped cell cluster"))?;
                let span = terminal_shapes.get(&(line, column)).map_or_else(
                    || if cell.width == CellWidth::Wide { 2 } else { 1 },
                    |shape| usize::from(shape.span),
                );
                let glyph_visual_column =
                    terminal_shapes
                        .get(&(line, column))
                        .map_or(visual_column, |shape| {
                            (column
                                ..column
                                    .saturating_add(usize::from(shape.span))
                                    .min(snapshot.grid.columns()))
                                .map(|logical| {
                                    usize::from(
                                        visual_map.lines[line].logical_to_visual_cell[logical],
                                    )
                                })
                                .min()
                                .unwrap_or(visual_column)
                        });
                let origin = cell_origin(layout, glyph_visual_column, line);
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
                                u32::from(layout.cell_px[0].get())
                                    * u32::try_from(span)
                                        .map_err(|_| ComposeError::Capacity("cluster clip"))?,
                                u32::from(layout.cell_px[1].get()),
                            ],
                            color: LinearColor::from_srgba8(glyph_color),
                            color_scale: if cell.flags.dim { 0.5 } else { 1.0 },
                        });
                    }
                    pen_x = pen_x.saturating_add(glyph.advance_26_6[0]);
                }
            }
            let underline_style = effective_underline_style(cell);
            let underlined = underline_style != UnderlineStyle::None;
            if underlined || cell.flags.strikeout {
                let origin = cell_origin(layout, visual_column, line);
                let span = if cell.width == CellWidth::Wide { 2 } else { 1 };
                if underlined {
                    let underline = if cell.underline_style == UnderlineStyle::None {
                        foreground
                    } else {
                        cell.underline_color
                            .map_or(foreground, |color| resolve_color(color, foreground, colors))
                    };
                    push_underline_primitives(
                        &mut rectangles,
                        &mut decoration_primitives,
                        underline_style,
                        origin,
                        layout,
                        span,
                        underline,
                    );
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
    }
    if cursor_visible
        && usize::from(snapshot.cursor.column) < snapshot.grid.columns()
        && usize::from(snapshot.cursor.line) < snapshot.grid.lines()
    {
        let origin = cell_origin(
            layout,
            usize::from(
                visual_map.lines[usize::from(snapshot.cursor.line)].logical_to_visual_cell
                    [usize::from(snapshot.cursor.column)],
            ),
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
    if let Some(tab_bar) = overlays.tab_bar {
        if let Some(bar) = tab_bar.bar {
            rectangles.push(RectangleInstance {
                origin_px: [bar.x as f32, bar.y as f32],
                size_px: [bar.width as f32, bar.height as f32],
                color: LinearColor::from_srgba8(TAB_BAR_BACKGROUND),
            });
        }
        let close_marker = tab_shaped_cache.get(&(TAB_CLOSE_MARK.to_owned(), FontStyle::Regular));
        for item in &tab_bar.items {
            if item.active {
                rectangles.push(RectangleInstance {
                    origin_px: [item.rect.x as f32, item.rect.y as f32],
                    size_px: [item.rect.width as f32, item.rect.height as f32],
                    color: LinearColor::from_srgba8(TAB_ACTIVE_BACKGROUND),
                });
            }
            if item.rect.width > 1 && item.rect.height > 14 {
                rectangles.push(RectangleInstance {
                    origin_px: [
                        item.rect.x.saturating_add(item.rect.width - 1) as f32,
                        item.rect.y.saturating_add(7) as f32,
                    ],
                    size_px: [1.0, item.rect.height.saturating_sub(14) as f32],
                    color: LinearColor::from_srgba8(TAB_DIVIDER),
                });
            }
            if item.active {
                let accent_height = item.rect.height.clamp(1, 2);
                rectangles.push(RectangleInstance {
                    origin_px: [
                        item.rect.x as f32,
                        item.rect
                            .y
                            .saturating_add(item.rect.height.saturating_sub(accent_height))
                            as f32,
                    ],
                    size_px: [item.rect.width as f32, accent_height as f32],
                    color: LinearColor::from_srgba8(TAB_ACCENT),
                });
            } else if item.unread && item.rect.width >= 20 {
                rectangles.push(RectangleInstance {
                    origin_px: [
                        item.rect.x.saturating_add(8) as f32,
                        item.rect.y.saturating_add(item.rect.height / 2) as f32,
                    ],
                    size_px: [3.0, 3.0],
                    color: LinearColor::from_srgba8(TAB_ACCENT),
                });
            }
            if let Some(shaped) = tab_shaped_cache.get(&(item.title.clone(), FontStyle::Regular)) {
                let title_left = item.rect.x.saturating_add(12);
                let title_right = item.close_rect.map_or_else(
                    || {
                        item.rect
                            .x
                            .saturating_add(item.rect.width)
                            .saturating_sub(12)
                    },
                    |close| close.x.saturating_sub(6),
                );
                let available_width = title_right.saturating_sub(title_left);
                let run_width = shaped
                    .glyphs
                    .iter()
                    .fold(0_i32, |width, glyph| {
                        width.saturating_add(glyph.advance_26_6[0])
                    })
                    .max(0)
                    / 64;
                let text_origin = title_left as i32
                    + (i32::try_from(available_width).unwrap_or(i32::MAX) - run_width).max(0) / 2;
                let mut pen_x = 0_i32;
                for glyph in &shaped.glyphs {
                    let Some(bitmap) = assets.get(&glyph.key) else {
                        continue;
                    };
                    let x = text_origin
                        + i32::from(bitmap.bitmap.bearing_px[0])
                        + (pen_x + glyph.offset_26_6[0]) / 64;
                    let y = item.rect.y.saturating_add(item.rect.height / 2) as i32
                        + i32::from(tab_metrics.baseline_px / 2)
                        - i32::from(bitmap.bitmap.bearing_px[1])
                        - glyph.offset_26_6[1] / 64;
                    if bitmap.bitmap.size_px != [0, 0] {
                        glyphs.push(GlyphPlacement {
                            key: glyph.key,
                            origin_px: [x, y],
                            clip_px: [title_left, item.rect.y, available_width, item.rect.height],
                            color: LinearColor::from_srgba8(if item.active {
                                TAB_ACTIVE_TEXT
                            } else if item.unread {
                                TAB_UNREAD_TEXT
                            } else {
                                TAB_INACTIVE_TEXT
                            }),
                            color_scale: 1.0,
                        });
                    }
                    pen_x = pen_x.saturating_add(glyph.advance_26_6[0]);
                }
            }
            if let (Some(close), Some(shaped)) = (item.close_rect, close_marker) {
                let run_width = shaped
                    .glyphs
                    .iter()
                    .fold(0_i32, |width, glyph| {
                        width.saturating_add(glyph.advance_26_6[0])
                    })
                    .max(0)
                    / 64;
                let text_origin = close.x as i32
                    + (i32::try_from(close.width).unwrap_or(i32::MAX) - run_width).max(0) / 2;
                let mut pen_x = 0_i32;
                for glyph in &shaped.glyphs {
                    let Some(bitmap) = assets.get(&glyph.key) else {
                        continue;
                    };
                    let x = text_origin
                        + i32::from(bitmap.bitmap.bearing_px[0])
                        + (pen_x + glyph.offset_26_6[0]) / 64;
                    let y = close.y.saturating_add(close.height / 2) as i32
                        + i32::from(tab_metrics.baseline_px / 2)
                        - i32::from(bitmap.bitmap.bearing_px[1])
                        - glyph.offset_26_6[1] / 64;
                    if bitmap.bitmap.size_px != [0, 0] {
                        glyphs.push(GlyphPlacement {
                            key: glyph.key,
                            origin_px: [x, y],
                            clip_px: [close.x, close.y, close.width, close.height],
                            color: LinearColor::from_srgba8(if item.active {
                                TAB_ACTIVE_TEXT
                            } else {
                                TAB_INACTIVE_TEXT
                            }),
                            color_scale: 1.0,
                        });
                    }
                    pen_x = pen_x.saturating_add(glyph.advance_26_6[0]);
                }
            }
        }
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
            usize::from(
                visual_map.lines[usize::from(preedit.anchor[1])].logical_to_visual_cell
                    [usize::from(preedit.anchor[0])],
            ),
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
                    color_scale: 1.0,
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
        glyph_assets: sorted_assets(assets),
        source_generation: snapshot.generation,
        font_generation: layout.font_generation,
        frame_key: leyline_gfx::FrameKey {
            snapshot_generation: snapshot.generation,
            layout_generation,
            font_generation: layout.font_generation,
            unicode_policy_generation: visual_map.policy_generation,
        },
    })
}

fn prepare_glyph_working_set(
    text: &mut TextSystem,
    snapshot: &FrameSnapshot,
    paste_confirmation: Option<&PasteConfirmationOverlay>,
    preedit: Option<&PreeditOverlay>,
    visual_map: &VisualGridMap,
) -> Result<(ShapedCache, TerminalShapes, GlyphAssets), ComposeError> {
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
                || is_bidi_control(cell.ch)
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
    let terminal_shapes = if paste_confirmation.is_none() && visual_map.bidi_enabled {
        prepare_terminal_runs(text, snapshot, visual_map, &mut key_set, &mut keys)?
    } else {
        TerminalShapes::new()
    };
    let placements = if paste_confirmation.is_some() {
        count_prepared_placements(snapshot, paste_confirmation, preedit, &shaped)
    } else {
        terminal_shapes.values().try_fold(0_usize, |total, shape| {
            total.checked_add(shape.run.glyphs.len())
        })
    };
    if placements.is_none_or(|count| count > MAX_PREPARED_GLYPHS) {
        return Err(ComposeError::Capacity("glyph placements"));
    }
    let assets = text
        .rasterize_working_set(&keys)?
        .into_iter()
        .map(|asset| (asset.key, asset))
        .collect();
    Ok((shaped, terminal_shapes, assets))
}

#[allow(clippy::too_many_lines)]
fn prepare_terminal_runs(
    text: &mut TextSystem,
    snapshot: &FrameSnapshot,
    visual_map: &VisualGridMap,
    key_set: &mut HashSet<GlyphKey>,
    keys: &mut Vec<GlyphKey>,
) -> Result<TerminalShapes, ComposeError> {
    let columns = snapshot.grid.columns();
    let mut result = TerminalShapes::new();
    for line in 0..snapshot.grid.lines() {
        let mut column = 0;
        while column < columns {
            let cell = &snapshot.cells[line * columns + column];
            if matches!(cell.width, CellWidth::Spacer | CellWidth::LeadingSpacer)
                || cell.flags.hidden
            {
                column += 1;
                continue;
            }
            let style = cell_style(cell);
            let emoji = cell_is_emoji(cell);
            let visual = usize::from(visual_map.lines[line].logical_to_visual_cell[column]);
            let rtl = visual_map.lines[line].atom_levels[visual] % 2 == 1;
            let mut run_text = String::new();
            let mut provenance = Vec::new();
            let mut next = column;
            while next < columns {
                let candidate = &snapshot.cells[line * columns + next];
                if matches!(
                    candidate.width,
                    CellWidth::Spacer | CellWidth::LeadingSpacer
                ) || candidate.flags.hidden
                    || is_bidi_control(candidate.ch)
                {
                    break;
                }
                let candidate_visual =
                    usize::from(visual_map.lines[line].logical_to_visual_cell[next]);
                if cell_style(candidate) != style
                    || cell_is_emoji(candidate) != emoji
                    || (visual_map.lines[line].atom_levels[candidate_visual] % 2 == 1) != rtl
                {
                    break;
                }
                provenance.push((run_text.len(), next));
                run_text.push(candidate.ch);
                if let Some(extra) = &candidate.zerowidth {
                    run_text.extend(extra.iter());
                }
                next += if candidate.width == CellWidth::Wide {
                    2
                } else {
                    1
                };
            }
            let shaped = text.shape_run_only(
                &run_text,
                style,
                if rtl {
                    TextDirection::RightToLeft
                } else {
                    TextDirection::LeftToRight
                },
            )?;
            let graphemes = run_text.grapheme_indices(true).collect::<Vec<_>>();
            for glyph in shaped.glyphs {
                let cluster = usize::try_from(glyph.cluster)
                    .map_err(|_| ComposeError::Snapshot("invalid shaping cluster"))?;
                let owner_index = provenance
                    .partition_point(|(offset, _)| *offset <= cluster)
                    .saturating_sub(1);
                let owner = provenance
                    .get(owner_index)
                    .ok_or(ComposeError::Snapshot("missing shaping provenance"))?
                    .1;
                let grapheme_end = graphemes
                    .iter()
                    .find(|(offset, grapheme)| {
                        *offset <= cluster && cluster < offset.saturating_add(grapheme.len())
                    })
                    .map_or(cluster.saturating_add(1), |(offset, grapheme)| {
                        offset.saturating_add(grapheme.len())
                    });
                let end_index = provenance
                    .partition_point(|(offset, _)| *offset < grapheme_end)
                    .saturating_sub(1);
                let end_owner = provenance.get(end_index).map_or(owner, |(_, cell)| *cell);
                let end_cell = &snapshot.cells[line * columns + end_owner];
                let logical_end = end_owner.saturating_add(if end_cell.width == CellWidth::Wide {
                    2
                } else {
                    1
                });
                let span = u8::try_from(logical_end.saturating_sub(owner))
                    .map_err(|_| ComposeError::Capacity("shape cluster span"))?;
                if key_set.insert(glyph.key) {
                    keys.push(glyph.key);
                }
                let shape = result
                    .entry((line, owner))
                    .or_insert_with(|| TerminalShape {
                        run: ShapedRun { glyphs: Vec::new() },
                        span,
                    });
                shape.span = shape.span.max(span);
                shape.run.glyphs.push(glyph);
            }
            if keys.len() > MAX_UNIQUE_GLYPHS {
                return Err(ComposeError::Capacity("unique glyphs"));
            }
            column = next.max(column + 1);
        }
    }
    Ok(result)
}

fn cell_style(cell: &SnapshotCell) -> FontStyle {
    match (cell.flags.bold, cell.flags.italic) {
        (true, true) => FontStyle::BoldItalic,
        (true, false) => FontStyle::Bold,
        (false, true) => FontStyle::Italic,
        (false, false) => FontStyle::Regular,
    }
}

fn cell_is_emoji(cell: &SnapshotCell) -> bool {
    std::iter::once(cell.ch)
        .chain(
            cell.zerowidth
                .iter()
                .flat_map(|chars| chars.iter().copied()),
        )
        .any(|ch| {
            matches!(
                ch,
                '\u{200d}'
                    | '\u{fe0f}'
                    | '\u{1f1e6}'..='\u{1f1ff}'
                    | '\u{1f300}'..='\u{1faff}'
                    | '\u{2600}'..='\u{27bf}'
            )
        })
}

const fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn prepare_tab_glyph_working_set(
    text: &mut TextSystem,
    tab_bar: Option<&crate::tab::TabBarPresentation>,
) -> Result<(ShapedCache, GlyphAssets), ComposeError> {
    let Some(tab_bar) = tab_bar else {
        return Ok((ShapedCache::new(), GlyphAssets::new()));
    };
    let mut requests = tab_bar
        .items
        .iter()
        .map(|item| item.title.clone())
        .collect::<HashSet<_>>();
    if tab_bar.items.iter().any(|item| item.close_rect.is_some()) {
        requests.insert(TAB_CLOSE_MARK.to_owned());
    }

    let mut shaped = ShapedCache::new();
    let mut keys = HashSet::new();
    for request in requests {
        let mut run = text.shape_cluster_only(&request, FontStyle::Regular)?;
        for glyph in &run.glyphs {
            keys.insert(glyph.key);
        }
        for glyph in &mut run.glyphs {
            glyph.key.font_generation |= TAB_FONT_KEY_NAMESPACE;
        }
        shaped.insert((request, FontStyle::Regular), run);
    }
    if keys.len() > MAX_UNIQUE_GLYPHS {
        return Err(ComposeError::Capacity("tab glyphs"));
    }
    let mut assets = GlyphAssets::new();
    for mut asset in text.rasterize_working_set(&keys.into_iter().collect::<Vec<_>>())? {
        asset.key.font_generation |= TAB_FONT_KEY_NAMESPACE;
        assets.insert(asset.key, asset);
    }
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
        total.checked_add(asset.bitmap.pixels.len())
    });
    validate_glyph_budget(glyphs.len(), assets.len(), bitmap_bytes)
}

fn sorted_assets(assets: GlyphAssets) -> Vec<GlyphAsset> {
    let mut assets = assets.into_values().collect::<Vec<_>>();
    assets.sort_by_key(|asset| {
        (
            asset.key.face.0,
            asset.key.glyph_id,
            asset.key.synthetic_bold,
            asset.key.synthetic_italic,
            match asset.key.presentation {
                leyline_text::GlyphPresentation::Text => 0_u8,
                leyline_text::GlyphPresentation::Emoji => 1_u8,
            },
        )
    });
    assets
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
                color_scale: 1.0,
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

fn effective_underline_style(cell: &SnapshotCell) -> UnderlineStyle {
    if cell.width == CellWidth::Spacer {
        UnderlineStyle::None
    } else if cell.underline_style != UnderlineStyle::None {
        cell.underline_style
    } else if cell.hyperlink.is_some() {
        UnderlineStyle::Single
    } else {
        UnderlineStyle::None
    }
}

#[allow(clippy::cast_precision_loss)]
fn push_underline_primitives(
    rectangles: &mut Vec<RectangleInstance>,
    count: &mut usize,
    mut style: UnderlineStyle,
    origin: [u32; 2],
    layout: &GridLayout,
    span: u32,
    color: u32,
) {
    let metrics = layout.cell_metrics;
    let width = u32::from(layout.cell_px[0].get()).saturating_mul(span);
    let height = u32::from(layout.cell_px[1].get());
    let thickness = u32::from(metrics.underline_thickness_px.get()).max(1);
    let y = u32::try_from(metrics.underline_y_px.max(0))
        .unwrap_or(0)
        .min(height - 1);
    if *count >= MAX_DECORATION_PRIMITIVES {
        style = UnderlineStyle::Single;
    }
    let mut push = |x: u32, y: u32, width: u32, height: u32| {
        if *count < MAX_DECORATION_PRIMITIVES && width != 0 && height != 0 {
            rectangles.push(RectangleInstance {
                origin_px: [x as f32, y as f32],
                size_px: [width as f32, height as f32],
                color: LinearColor::from_srgba8(color),
            });
            *count += 1;
        }
    };
    match style {
        UnderlineStyle::None => {}
        UnderlineStyle::Double if y >= thickness.saturating_mul(2) => {
            push(origin[0], origin[1] + y - thickness * 2, width, thickness);
            push(origin[0], origin[1] + y, width, thickness.min(height - y));
        }
        UnderlineStyle::Curly if width >= 4 && height >= 3 => {
            let amplitude = thickness.max(1).min((height - 1) / 2).max(1);
            for x in 0..width {
                let phase = (origin[0] + x) % (amplitude * 4);
                let offset = if phase < amplitude {
                    i64::from(phase)
                } else if phase < amplitude * 3 {
                    i64::from(amplitude * 2) - i64::from(phase)
                } else {
                    i64::from(phase) - i64::from(amplitude * 4)
                };
                let wave_y = i64::from(y) + offset;
                let wave_y = u32::try_from(wave_y.clamp(0, i64::from(height - 1))).unwrap_or(0);
                push(
                    origin[0] + x,
                    origin[1] + wave_y,
                    1,
                    thickness.min(height - wave_y),
                );
            }
        }
        UnderlineStyle::Single | UnderlineStyle::Double | UnderlineStyle::Curly => {
            push(origin[0], origin[1] + y, width, thickness.min(height - y));
        }
        UnderlineStyle::Dotted => {
            let dot = thickness.max(1);
            let step = dot.saturating_mul(2);
            let mut x = 0;
            while x < width {
                push(
                    origin[0] + x,
                    origin[1] + y,
                    dot.min(width - x),
                    dot.min(height - y),
                );
                x = x.saturating_add(step);
            }
        }
        UnderlineStyle::Dashed => {
            let dash = thickness.saturating_mul(3).max(1);
            let gap = thickness.saturating_mul(2).max(1);
            let period = dash + gap;
            let phase = origin[0] % period;
            let mut x = period - phase;
            if phase < dash {
                push(
                    origin[0],
                    origin[1] + y,
                    (dash - phase).min(width),
                    thickness.min(height - y),
                );
            }
            while x < width {
                push(
                    origin[0] + x,
                    origin[1] + y,
                    dash.min(width - x),
                    thickness.min(height - y),
                );
                x = x.saturating_add(period);
            }
        }
    }
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
    use std::sync::Arc;

    use super::{
        cursor_glyph_color, dim, downgrade_color_working_set, effective_underline_style,
        push_underline_primitives, resolve_color, validate_glyph_budget,
    };
    use crate::config::CursorStyle;
    use crate::terminal::{CellFlags, CellWidth, SnapshotCell, TerminalColor, UnderlineStyle};
    use leyline_text::{
        FaceId, GlyphAsset, GlyphBitmap, GlyphFormat, GlyphKey, GlyphPresentation,
        MAX_GLYPH_BITMAP_BYTES, MAX_GLYPH_BITMAPS, MAX_PREPARED_GLYPHS,
    };

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
    fn color_capacity_fallback_preserves_keys_and_uses_alpha_coverage() {
        let key = GlyphKey {
            font_generation: 3,
            face: FaceId(2),
            glyph_id: 1,
            synthetic_bold: false,
            synthetic_italic: false,
            presentation: GlyphPresentation::Emoji,
        };
        let mut scene = leyline_gfx::SceneData {
            clear: leyline_gfx::LinearColor::from_srgba8(0),
            rectangles: Vec::new(),
            glyphs: Vec::new(),
            glyph_assets: vec![GlyphAsset {
                key,
                bitmap: GlyphBitmap {
                    format: GlyphFormat::ColorSrgba8,
                    size_px: [2, 1],
                    bearing_px: [0, 0],
                    advance_26_6: 64,
                    pixels: Arc::from([10, 20, 30, 40, 50, 60, 70, 80]),
                },
            }],
            source_generation: 1,
            font_generation: 3,
            frame_key: leyline_gfx::FrameKey::default(),
        };
        assert!(downgrade_color_working_set(&mut scene));
        assert_eq!(scene.glyph_assets[0].key, key);
        assert_eq!(scene.glyph_assets[0].bitmap.format, GlyphFormat::Gray8);
        assert_eq!(&*scene.glyph_assets[0].bitmap.pixels, &[40, 80]);
        assert!(!downgrade_color_working_set(&mut scene));
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

    #[test]
    fn osc_8_hyperlinks_are_underlined() {
        let mut cell = SnapshotCell {
            ch: 'x',
            zerowidth: None,
            foreground: TerminalColor::Named(256),
            background: TerminalColor::Named(257),
            underline_color: None,
            underline_style: UnderlineStyle::None,
            flags: CellFlags::default(),
            width: CellWidth::Narrow,
            hyperlink: None,
        };
        assert_eq!(effective_underline_style(&cell), UnderlineStyle::None);

        cell.hyperlink = Some(0);
        assert_eq!(effective_underline_style(&cell), UnderlineStyle::Single);

        cell.width = CellWidth::Spacer;
        assert_eq!(effective_underline_style(&cell), UnderlineStyle::None);
    }

    #[test]
    fn underline_styles_produce_bounded_physical_primitives() {
        let metrics = leyline_text::CellMetrics {
            width_px: std::num::NonZeroU16::new(9).unwrap(),
            height_px: std::num::NonZeroU16::new(18).unwrap(),
            baseline_px: 14,
            underline_y_px: 15,
            underline_thickness_px: std::num::NonZeroU16::new(1).unwrap(),
            strike_y_px: 9,
            strike_thickness_px: std::num::NonZeroU16::new(1).unwrap(),
        };
        let layout = crate::layout::GridLayout::calculate(
            leyline_gfx::LogicalSize {
                width: 90,
                height: 36,
            },
            leyline_gfx::Scale120::ONE,
            [0, 0],
            metrics,
            1,
        )
        .unwrap();
        for (style, minimum) in [
            (UnderlineStyle::Single, 1),
            (UnderlineStyle::Double, 2),
            (UnderlineStyle::Curly, 4),
            (UnderlineStyle::Dotted, 2),
            (UnderlineStyle::Dashed, 1),
        ] {
            let mut rectangles = Vec::new();
            let mut count = 0;
            push_underline_primitives(
                &mut rectangles,
                &mut count,
                style,
                [9, 0],
                &layout,
                2,
                0xffff_ffff,
            );
            assert!(rectangles.len() >= minimum, "{style:?}");
            assert_eq!(rectangles.len(), count);
            assert!(rectangles.iter().all(|rectangle| {
                rectangle.origin_px[0] >= 9.0
                    && rectangle.origin_px[1] >= 0.0
                    && rectangle.origin_px[0] + rectangle.size_px[0] <= 27.0
                    && rectangle.origin_px[1] + rectangle.size_px[1] <= 18.0
            }));
        }
    }
}

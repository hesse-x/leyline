use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    config::{Color, ScrollbarConfig, ScrollbarMode},
    layout::GridLayout,
    terminal::{FrameSnapshot, SelectionKind, SelectionPoint},
};

const MAX_PREEDIT_BYTES: usize = 4096;
const MAX_PREEDIT_SCALARS: usize = 1024;
const MAX_IME_COMMIT_BYTES: usize = 64 * 1024;
const MULTI_CLICK_MS: u32 = 400;
const SCROLLBAR_VISIBILITY: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl PixelRect {
    #[must_use]
    pub fn contains(self, point: [f64; 2]) -> bool {
        point[0] >= self.x
            && point[0] < self.x + self.width
            && point[1] >= self.y
            && point[1] < self.y + self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarGeometry {
    pub hit: PixelRect,
    pub track: PixelRect,
    pub thumb: PixelRect,
    travel: f64,
    history_size: usize,
}

impl ScrollbarGeometry {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn calculate(
        snapshot: &FrameSnapshot,
        layout: &GridLayout,
        config: &ScrollbarConfig,
        scale_120: u32,
    ) -> Option<Self> {
        if config.mode == ScrollbarMode::Hidden || scale_120 == 0 {
            return None;
        }
        let scale = f64::from(scale_120) / 120.0;
        let grid_right = f64::from(layout.content_origin_px[0])
            + f64::from(layout.cell_px[0].get()) * f64::from(layout.grid.columns.get());
        let track = PixelRect {
            x: grid_right,
            y: f64::from(layout.content_origin_px[1]),
            width: (f64::from(layout.viewport_px.width) - grid_right).max(0.0),
            height: f64::from(layout.cell_px[1].get()) * f64::from(layout.grid.lines.get()),
        };
        let hit_width = (config.hit_width * scale).round().max(1.0).min(track.width);
        let visual_width = (config.width * scale).round().max(1.0).min(track.width);
        let hit = PixelRect {
            x: f64::from(layout.viewport_px.width) - hit_width,
            width: hit_width,
            ..track
        };
        let total = snapshot.history_size.saturating_add(snapshot.grid.lines());
        let ratio = if total == 0 {
            1.0
        } else {
            snapshot.grid.lines() as f64 / total as f64
        };
        let min_thumb = (config.min_thumb_size * scale).round().max(1.0);
        let thumb_height = (track.height * ratio).max(min_thumb).min(track.height);
        let travel = (track.height - thumb_height).max(0.0);
        let position = if snapshot.history_size == 0 {
            1.0
        } else {
            snapshot
                .history_size
                .saturating_sub(snapshot.display_offset) as f64
                / snapshot.history_size as f64
        };
        let thumb = PixelRect {
            x: f64::from(layout.viewport_px.width) - visual_width - 2.0 * scale,
            y: track.y + (position.clamp(0.0, 1.0) * travel).round(),
            width: visual_width,
            height: thumb_height,
        };
        Some(Self {
            hit,
            track,
            thumb,
            travel,
            history_size: snapshot.history_size,
        })
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn offset_for_pointer(self, pointer_y: f64, grab_offset: f64) -> usize {
        if self.travel <= 0.0 || self.history_size == 0 {
            return 0;
        }
        let top = (pointer_y - grab_offset).clamp(self.track.y, self.track.y + self.travel);
        let ratio = (top - self.track.y) / self.travel;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            (self.history_size as f64 * (1.0 - ratio)).round() as usize
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollbarInteraction {
    Idle,
    Hover,
    Dragging { grab_offset_px: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarPresentation {
    pub track: PixelRect,
    pub thumb: PixelRect,
    pub track_color: Color,
    pub thumb_color: Color,
}

#[derive(Clone, Debug)]
pub struct ScrollbarController {
    interaction: ScrollbarInteraction,
    visible_until: Option<Instant>,
}

impl Default for ScrollbarController {
    fn default() -> Self {
        Self {
            interaction: ScrollbarInteraction::Idle,
            visible_until: None,
        }
    }
}

impl ScrollbarController {
    #[must_use]
    pub const fn interaction(&self) -> ScrollbarInteraction {
        self.interaction
    }

    pub fn pointer_motion(
        &mut self,
        point: [f64; 2],
        geometry: ScrollbarGeometry,
        now: Instant,
    ) -> Option<usize> {
        if let ScrollbarInteraction::Dragging { grab_offset_px } = self.interaction {
            self.visible_until = Some(now + SCROLLBAR_VISIBILITY);
            return Some(geometry.offset_for_pointer(point[1], grab_offset_px));
        }
        self.interaction = if geometry.hit.contains(point) {
            ScrollbarInteraction::Hover
        } else {
            ScrollbarInteraction::Idle
        };
        if self.interaction == ScrollbarInteraction::Hover {
            self.visible_until = Some(now + SCROLLBAR_VISIBILITY);
        }
        None
    }

    #[must_use]
    pub fn press(
        &mut self,
        point: [f64; 2],
        geometry: ScrollbarGeometry,
        viewport_lines: usize,
        current_offset: usize,
        now: Instant,
    ) -> Option<usize> {
        if !geometry.hit.contains(point) {
            return None;
        }
        self.visible_until = Some(now + SCROLLBAR_VISIBILITY);
        let over_thumb =
            point[1] >= geometry.thumb.y && point[1] < geometry.thumb.y + geometry.thumb.height;
        if over_thumb && geometry.travel > 0.0 {
            self.interaction = ScrollbarInteraction::Dragging {
                grab_offset_px: point[1] - geometry.thumb.y,
            };
            return Some(current_offset);
        }
        let page = viewport_lines.saturating_sub(1).max(1);
        Some(if point[1] < geometry.thumb.y {
            current_offset
                .saturating_add(page)
                .min(geometry.history_size)
        } else {
            current_offset.saturating_sub(page)
        })
    }

    pub fn release(&mut self) -> bool {
        let consumed = matches!(self.interaction, ScrollbarInteraction::Dragging { .. });
        self.interaction = ScrollbarInteraction::Idle;
        consumed
    }

    pub fn cancel(&mut self) {
        self.interaction = ScrollbarInteraction::Idle;
    }

    pub fn note_scroll(&mut self, now: Instant) {
        self.visible_until = Some(now + SCROLLBAR_VISIBILITY);
    }

    pub fn expire(&mut self, now: Instant) -> bool {
        if self.visible_until.is_some_and(|deadline| now >= deadline)
            && self.interaction == ScrollbarInteraction::Idle
        {
            self.visible_until = None;
            return true;
        }
        false
    }

    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.visible_until
    }

    #[must_use]
    pub fn presentation(
        &self,
        geometry: ScrollbarGeometry,
        snapshot: &FrameSnapshot,
        config: &ScrollbarConfig,
        now: Instant,
    ) -> Option<ScrollbarPresentation> {
        let visible = config.mode == ScrollbarMode::Always
            || matches!(
                self.interaction,
                ScrollbarInteraction::Hover | ScrollbarInteraction::Dragging { .. }
            )
            || snapshot.display_offset > 0
            || self.visible_until.is_some_and(|deadline| now < deadline);
        (visible && (snapshot.history_size > 0 || config.mode == ScrollbarMode::Always)).then_some(
            ScrollbarPresentation {
                track: geometry.track,
                thumb: geometry.thumb,
                track_color: config.track,
                thumb_color: if matches!(
                    self.interaction,
                    ScrollbarInteraction::Hover | ScrollbarInteraction::Dragging { .. }
                ) {
                    config.thumb_hover
                } else {
                    config.thumb
                },
            },
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClickTracker {
    last: Option<ClickRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClickRecord {
    button: u32,
    point: SelectionPoint,
    time_ms: u32,
    count: u8,
}

impl ClickTracker {
    #[must_use]
    pub fn register(&mut self, button: u32, point: SelectionPoint, time_ms: u32) -> SelectionKind {
        let count = self.last.map_or(1, |last| {
            if last.button == button
                && last.point == point
                && time_ms.wrapping_sub(last.time_ms) <= MULTI_CLICK_MS
            {
                (last.count % 3) + 1
            } else {
                1
            }
        });
        self.last = Some(ClickRecord {
            button,
            point,
            time_ms,
            count,
        });
        match count {
            2 => SelectionKind::Semantic,
            3 => SelectionKind::Lines,
            _ => SelectionKind::Simple,
        }
    }

    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkCandidate {
    pub snapshot_generation: u64,
    pub hyperlink: u16,
    pub point: SelectionPoint,
    pub modifiers: leyline_gfx::ModifiersState,
}

impl LinkCandidate {
    #[must_use]
    pub fn matches(
        &self,
        snapshot_generation: u64,
        hyperlink: u16,
        point: SelectionPoint,
        modifiers: leyline_gfx::ModifiersState,
    ) -> bool {
        self.snapshot_generation == snapshot_generation
            && self.hyperlink == hyperlink
            && self.point == point
            && self.modifiers == modifiers
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreeditOverlay {
    pub snapshot_generation: u64,
    pub revision: u64,
    pub anchor: [u16; 2],
    pub text: Arc<str>,
    pub cursor: PreeditCursor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreeditCursor {
    Hidden,
    Caret(u16),
    Selection(Range<u16>),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImeOutboundState {
    pub commit_serial: u32,
    pub dirty: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImeTransaction {
    pub preedit: Option<(String, Option<(i32, i32)>)>,
    pub commit: Option<String>,
    pub delete_surrounding: Option<(u32, u32)>,
}

#[derive(Clone, Debug, Default)]
pub struct ImeState {
    active: bool,
    pending: ImeTransaction,
    revision: u64,
    pub outbound: ImeOutboundState,
    pub preedit: Option<PreeditOverlay>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImeDone {
    pub commit: Option<Vec<u8>>,
    pub delete_surrounding: Option<(u32, u32)>,
    pub outbound_resend_required: bool,
}

#[allow(clippy::missing_errors_doc)]
impl ImeState {
    pub fn activate(&mut self) {
        self.active = true;
        self.outbound.dirty = true;
    }
    pub fn deactivate(&mut self) {
        self.active = false;
        self.pending = ImeTransaction::default();
        self.preedit = None;
    }
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }
    #[must_use]
    pub fn has_preedit(&self) -> bool {
        self.pending
            .preedit
            .as_ref()
            .is_some_and(|(text, _)| !text.is_empty())
            || self
                .preedit
                .as_ref()
                .is_some_and(|preedit| !preedit.text.is_empty())
    }
    pub fn reanchor_preedit(&mut self, generation: u64, anchor: [u16; 2]) {
        if let Some(preedit) = self.preedit.as_mut() {
            preedit.snapshot_generation = generation;
            preedit.anchor = anchor;
        }
    }
    pub fn sent_commit(&mut self) -> Result<u32, ImeError> {
        self.outbound.commit_serial = self
            .outbound
            .commit_serial
            .checked_add(1)
            .ok_or(ImeError::SerialOverflow)?;
        self.outbound.dirty = false;
        Ok(self.outbound.commit_serial)
    }
    pub fn record_commit_serial(&mut self, serial: u32) {
        self.outbound.commit_serial = serial;
        self.outbound.dirty = false;
    }
    pub fn preedit_string(
        &mut self,
        text: String,
        cursor: Option<(i32, i32)>,
    ) -> Result<(), ImeError> {
        self.ensure_active()?;
        if text.len() > MAX_PREEDIT_BYTES || text.chars().count() > MAX_PREEDIT_SCALARS {
            return Err(ImeError::PreeditTooLarge);
        }
        if let Some((begin, end)) = cursor {
            validate_cursor(&text, begin, end)?;
        }
        self.pending.preedit = Some((text, cursor));
        Ok(())
    }
    pub fn commit_string(&mut self, text: String) -> Result<(), ImeError> {
        self.ensure_active()?;
        if text.len() > MAX_IME_COMMIT_BYTES || text.contains('\0') {
            return Err(ImeError::CommitTooLarge);
        }
        self.pending.commit = Some(text);
        Ok(())
    }
    pub fn delete_surrounding_text(&mut self, before: u32, after: u32) -> Result<(), ImeError> {
        self.ensure_active()?;
        self.pending.delete_surrounding = Some((before, after));
        Ok(())
    }
    pub fn done(
        &mut self,
        serial: u32,
        generation: u64,
        anchor: [u16; 2],
    ) -> Result<ImeDone, ImeError> {
        self.ensure_active()?;
        let pending = std::mem::take(&mut self.pending);
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(ImeError::RevisionOverflow)?;
        self.preedit = pending.preedit.map(|(text, cursor)| PreeditOverlay {
            snapshot_generation: generation,
            revision: self.revision,
            anchor,
            cursor: cursor.map_or(PreeditCursor::Hidden, |(begin, end)| {
                if begin == end {
                    PreeditCursor::Caret(u16::try_from(begin).unwrap_or(u16::MAX))
                } else {
                    PreeditCursor::Selection(
                        u16::try_from(begin).unwrap_or(u16::MAX)
                            ..u16::try_from(end).unwrap_or(u16::MAX),
                    )
                }
            }),
            text: Arc::from(text),
        });
        let mismatch = serial != self.outbound.commit_serial;
        if mismatch {
            self.outbound.dirty = true;
        }
        Ok(ImeDone {
            commit: pending.commit.map(String::into_bytes),
            delete_surrounding: pending.delete_surrounding,
            outbound_resend_required: mismatch,
        })
    }
    fn ensure_active(&self) -> Result<(), ImeError> {
        if self.active {
            Ok(())
        } else {
            Err(ImeError::Inactive)
        }
    }
}

fn validate_cursor(text: &str, begin: i32, end: i32) -> Result<(), ImeError> {
    let begin = usize::try_from(begin).map_err(|_| ImeError::InvalidCursor)?;
    let end = usize::try_from(end).map_err(|_| ImeError::InvalidCursor)?;
    if begin > end
        || end > text.len()
        || !text.is_char_boundary(begin)
        || !text.is_char_boundary(end)
    {
        return Err(ImeError::InvalidCursor);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ImeError {
    #[error("text input is inactive")]
    Inactive,
    #[error("preedit exceeds its hard limit")]
    PreeditTooLarge,
    #[error("IME commit exceeds its hard limit or contains NUL")]
    CommitTooLarge,
    #[error("preedit cursor is not a valid UTF-8 byte range")]
    InvalidCursor,
    #[error("IME serial overflow")]
    SerialOverflow,
    #[error("IME revision overflow")]
    RevisionOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU16;
    #[test]
    fn done_applies_commit_even_when_serial_mismatches() {
        let mut ime = ImeState::default();
        ime.activate();
        assert_eq!(ime.sent_commit().unwrap(), 1);
        ime.commit_string("中文".into()).unwrap();
        let done = ime.done(99, 7, [2, 3]).unwrap();
        assert_eq!(done.commit.as_deref(), Some("中文".as_bytes()));
        assert!(done.outbound_resend_required && ime.outbound.dirty);
    }
    #[test]
    fn transaction_is_invisible_until_done_and_preedit_never_commits() {
        let mut ime = ImeState::default();
        ime.activate();
        ime.preedit_string("中".into(), Some((0, 3))).unwrap();
        assert!(ime.has_preedit());
        assert!(ime.preedit.is_none());
        let done = ime.done(0, 4, [1, 1]).unwrap();
        assert!(done.commit.is_none());
        assert!(ime.has_preedit());
        assert_eq!(ime.preedit.as_ref().unwrap().text.as_ref(), "中");

        ime.preedit_string(String::new(), None).unwrap();
        ime.done(0, 4, [1, 1]).unwrap();
        assert!(!ime.has_preedit());
    }
    #[test]
    fn cursor_must_use_utf8_boundaries() {
        let mut ime = ImeState::default();
        ime.activate();
        assert_eq!(
            ime.preedit_string("中".into(), Some((1, 2))),
            Err(ImeError::InvalidCursor)
        );
    }

    #[test]
    fn click_tracker_requires_same_cell_button_and_deadline() {
        let point = SelectionPoint { column: 2, line: 1 };
        let mut clicks = ClickTracker::default();
        assert_eq!(clicks.register(1, point, 10), SelectionKind::Simple);
        assert_eq!(clicks.register(1, point, 410), SelectionKind::Semantic);
        assert_eq!(clicks.register(1, point, 411), SelectionKind::Lines);
        assert_eq!(clicks.register(1, point, 412), SelectionKind::Simple);
        assert_eq!(clicks.register(2, point, 413), SelectionKind::Simple);
        assert_eq!(
            clicks.register(2, SelectionPoint { column: 3, line: 1 }, 414),
            SelectionKind::Simple
        );
        assert_eq!(clicks.register(2, point, 900), SelectionKind::Simple);
    }

    #[test]
    fn link_candidate_rejects_any_stale_release_property() {
        let point = SelectionPoint { column: 2, line: 1 };
        let modifiers = leyline_gfx::ModifiersState {
            control: true,
            ..leyline_gfx::ModifiersState::default()
        };
        let candidate = LinkCandidate {
            snapshot_generation: 7,
            hyperlink: 3,
            point,
            modifiers,
        };
        assert!(candidate.matches(7, 3, point, modifiers));
        assert!(!candidate.matches(8, 3, point, modifiers));
        assert!(!candidate.matches(7, 4, point, modifiers));
    }

    #[test]
    fn scrollbar_maps_bottom_middle_and_top_without_delta_accumulation() {
        let geometry = ScrollbarGeometry {
            hit: PixelRect {
                x: 90.0,
                y: 0.0,
                width: 10.0,
                height: 100.0,
            },
            track: PixelRect {
                x: 90.0,
                y: 0.0,
                width: 10.0,
                height: 100.0,
            },
            thumb: PixelRect {
                x: 94.0,
                y: 80.0,
                width: 4.0,
                height: 20.0,
            },
            travel: 80.0,
            history_size: 10_000,
        };
        assert_eq!(geometry.offset_for_pointer(80.0, 0.0), 0);
        assert_eq!(geometry.offset_for_pointer(40.0, 0.0), 5_000);
        assert_eq!(geometry.offset_for_pointer(0.0, 0.0), 10_000);
    }

    #[test]
    fn scrollbar_thumb_uses_the_full_gutter_as_its_drag_hit_width() {
        let geometry = ScrollbarGeometry {
            hit: PixelRect {
                x: 88.0,
                y: 0.0,
                width: 12.0,
                height: 100.0,
            },
            track: PixelRect {
                x: 88.0,
                y: 0.0,
                width: 12.0,
                height: 100.0,
            },
            thumb: PixelRect {
                x: 94.0,
                y: 80.0,
                width: 4.0,
                height: 20.0,
            },
            travel: 80.0,
            history_size: 10_000,
        };
        let mut scrollbar = ScrollbarController::default();

        assert_eq!(
            scrollbar.press([89.0, 90.0], geometry, 24, 0, Instant::now()),
            Some(0)
        );
        assert_eq!(
            scrollbar.interaction(),
            ScrollbarInteraction::Dragging {
                grab_offset_px: 10.0
            }
        );
        assert_eq!(
            scrollbar.pointer_motion([89.0, 50.0], geometry, Instant::now()),
            Some(5_000)
        );
    }

    #[test]
    fn scrollbar_geometry_handles_empty_history_and_short_tracks() {
        let grid = crate::terminal::GridSize::new(10, 2).unwrap();
        let snapshot = FrameSnapshot {
            generation: 1,
            content_revision: 1,
            active_buffer: crate::terminal::SearchBuffer::Normal,
            grid,
            cells: vec![
                crate::terminal::SnapshotCell {
                    ch: ' ',
                    zerowidth: None,
                    foreground: crate::terminal::TerminalColor::Named(256),
                    background: crate::terminal::TerminalColor::Named(257),
                    underline_color: None,
                    underline_style: crate::terminal::UnderlineStyle::None,
                    flags: crate::terminal::CellFlags::default(),
                    width: crate::terminal::CellWidth::Narrow,
                    hyperlink: None,
                };
                grid.columns() * grid.lines()
            ]
            .into(),
            cursor: crate::terminal::CursorSnapshot {
                column: 0,
                line: 0,
                visible: true,
                shape: crate::terminal::CursorShape::Block,
                blink: crate::terminal::CursorBlink::Steady,
            },
            modes: crate::terminal::TerminalModes::default(),
            display_offset: 0,
            history_size: 0,
            title: None,
            hyperlinks: Arc::from([]),
        };
        let metrics = leyline_text::CellMetrics {
            width_px: NonZeroU16::new(8).unwrap(),
            height_px: NonZeroU16::new(10).unwrap(),
            baseline_px: 8,
            underline_y_px: 9,
            underline_thickness_px: NonZeroU16::new(1).unwrap(),
            strike_y_px: 5,
            strike_thickness_px: NonZeroU16::new(1).unwrap(),
        };
        let layout = GridLayout::calculate(
            leyline_gfx::LogicalSize {
                width: 120,
                height: 40,
            },
            leyline_gfx::Scale120::ONE,
            [4, 4],
            metrics,
            1,
        )
        .unwrap();
        let geometry = ScrollbarGeometry::calculate(
            &snapshot,
            &layout,
            &crate::config::EffectiveConfig::default().scrollbar,
            120,
        )
        .unwrap();
        assert_eq!(geometry.offset_for_pointer(geometry.track.y, 0.0), 0);
        assert!((geometry.thumb.height - geometry.track.height).abs() < f64::EPSILON);
    }
}

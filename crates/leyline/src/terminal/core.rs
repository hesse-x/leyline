use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::{Config, TermMode, cell::Flags, test::TermSize},
    vte::ansi::{self, Color},
};

use super::snapshot::{
    CellFlags, CellWidth, CursorSnapshot, FrameSnapshot, GridSize, MouseEncoding, MouseProtocol,
    ProjectedSelection, SelectionKind, SelectionPoint, SelectionSide, SnapshotCell,
    SnapshotHyperlink, TerminalColor, TerminalModes,
};
use crate::{
    app::event::ByteBatch,
    security::{
        MAX_HYPERLINK_BYTES, MAX_HYPERLINKS, MAX_PTY_REPLY_BYTES, MAX_ZERO_WIDTH_PER_CELL,
        MAX_ZERO_WIDTH_TOTAL, PolicyDecision, validate_title,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalAction {
    SetTitle(Arc<str>),
    Bell,
    WriteToPty(Vec<u8>),
    ClipboardRequestRejected,
    UnsupportedSequence,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalDelta {
    pub dirty: bool,
    pub actions: usize,
    pub audit: ParseAuditDelta,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ParseAuditDelta {
    pub unknown_sequences: u32,
    pub rejected_actions: u32,
    pub truncated_sequences: u32,
    pub reply_bytes: usize,
}

#[derive(Clone, Default)]
struct Listener(Rc<RefCell<Vec<Event>>>);
impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

pub struct TerminalCoreAdapter {
    term: Term<Listener>,
    parser: ansi::Processor,
    events: Rc<RefCell<Vec<Event>>>,
    actions: Vec<TerminalAction>,
    generation: u64,
    size: GridSize,
    title: Option<Arc<str>>,
    cached: RefCell<Option<FrameSnapshot>>,
    selection_revision: u64,
    _main_thread: Rc<()>,
}

#[allow(clippy::missing_errors_doc)]
impl TerminalCoreAdapter {
    pub fn new(size: GridSize, history_lines: usize) -> Result<Self, TerminalError> {
        let listener = Listener::default();
        let events = Rc::clone(&listener.0);
        let config = Config {
            scrolling_history: history_lines.min(100_000),
            osc52: alacritty_terminal::term::Osc52::Disabled,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &TermSize::new(size.columns(), size.lines()),
            listener,
        );
        Ok(Self {
            term,
            parser: ansi::Processor::new(),
            events,
            actions: Vec::new(),
            generation: 0,
            size,
            title: None,
            cached: RefCell::new(None),
            selection_revision: 0,
            _main_thread: Rc::new(()),
        })
    }

    pub fn advance(&mut self, bytes: &[u8]) -> Result<TerminalDelta, TerminalError> {
        if bytes.is_empty() {
            return Ok(TerminalDelta::default());
        }
        if bytes.len() > ByteBatch::MAX_LEN {
            return Err(TerminalError::BatchTooLarge(bytes.len()));
        }
        self.parser.advance(&mut self.term, bytes);
        let parser_audit = self.parser.take_audit_delta();
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TerminalError::GenerationOverflow)?;
        self.cached.get_mut().take();
        let event_audit = self.collect_events();
        Ok(TerminalDelta {
            dirty: true,
            actions: self.actions.len(),
            audit: ParseAuditDelta {
                unknown_sequences: event_audit
                    .unknown_sequences
                    .saturating_add(parser_audit.unknown_sequences),
                rejected_actions: event_audit
                    .rejected_actions
                    .saturating_add(parser_audit.rejected_actions),
                truncated_sequences: parser_audit.truncated_sequences,
                reply_bytes: event_audit.reply_bytes,
            },
        })
    }

    pub fn resize(&mut self, size: GridSize) -> Result<TerminalDelta, TerminalError> {
        if size == self.size {
            return Ok(TerminalDelta::default());
        }
        self.term
            .resize(TermSize::new(size.columns(), size.lines()));
        self.size = size;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TerminalError::GenerationOverflow)?;
        self.cached.get_mut().take();
        Ok(TerminalDelta {
            dirty: true,
            actions: 0,
            audit: ParseAuditDelta::default(),
        })
    }

    pub fn snapshot(&self) -> Result<FrameSnapshot, TerminalError> {
        if let Some(snapshot) = self.cached.borrow().as_ref() {
            return Ok(snapshot.clone());
        }
        let content = self.term.renderable_content();
        let mut cells = Vec::with_capacity(
            self.size
                .columns()
                .checked_mul(self.size.lines())
                .ok_or(TerminalError::SnapshotTooLarge)?,
        );
        let mut links = Vec::<SnapshotHyperlink>::new();
        let mut link_ids = HashMap::<(String, String), u16>::new();
        let mut zero_width_total = 0_usize;
        for indexed in content.display_iter {
            let cell = indexed.cell;
            let hyperlink = if let Some(link) = cell.hyperlink() {
                if link.id().len() > MAX_HYPERLINK_BYTES || link.uri().len() > MAX_HYPERLINK_BYTES {
                    None
                } else {
                    let key = (link.id().to_owned(), link.uri().to_owned());
                    if let Some(id) = link_ids.get(&key) {
                        Some(*id)
                    } else if links.len() >= MAX_HYPERLINKS {
                        None
                    } else {
                        let id = u16::try_from(links.len())
                            .map_err(|_| TerminalError::TooManyHyperlinks)?;
                        links.push(SnapshotHyperlink {
                            id: Arc::from(key.0.as_str()),
                            uri: Arc::from(key.1.as_str()),
                        });
                        link_ids.insert(key, id);
                        Some(id)
                    }
                }
            } else {
                None
            };
            let zerowidth = cell.zerowidth().filter(|value| {
                !value.is_empty()
                    && value.len() <= MAX_ZERO_WIDTH_PER_CELL
                    && zero_width_total
                        .checked_add(value.len())
                        .is_some_and(|total| total <= MAX_ZERO_WIDTH_TOTAL)
            });
            if let Some(value) = zerowidth {
                zero_width_total += value.len();
            }
            cells.push(SnapshotCell {
                ch: cell.c,
                zerowidth: zerowidth.map(Arc::from),
                foreground: map_color(cell.fg),
                background: map_color(cell.bg),
                underline_color: cell.underline_color().map(map_color),
                flags: CellFlags {
                    bold: cell.flags.contains(Flags::BOLD),
                    dim: cell.flags.contains(Flags::DIM),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                    inverse: cell.flags.contains(Flags::INVERSE),
                    hidden: cell.flags.contains(Flags::HIDDEN),
                    strikeout: cell.flags.contains(Flags::STRIKEOUT),
                },
                width: if cell.flags.contains(Flags::WIDE_CHAR) {
                    CellWidth::Wide
                } else if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    CellWidth::LeadingSpacer
                } else if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    CellWidth::Spacer
                } else {
                    CellWidth::Narrow
                },
                hyperlink,
            });
        }
        while cells.len() < self.size.columns() * self.size.lines() {
            cells.push(default_cell());
        }
        if cells.len() > self.size.columns() * self.size.lines() {
            cells.truncate(self.size.columns() * self.size.lines());
        }
        let mode = *self.term.mode();
        let cursor = content.cursor;
        let snapshot = FrameSnapshot {
            generation: self.generation,
            grid: self.size,
            cells: cells.into(),
            cursor: CursorSnapshot {
                column: u16::try_from(cursor.point.column.0).unwrap_or(u16::MAX),
                line: u16::try_from(cursor.point.line.0.max(0)).unwrap_or(u16::MAX),
                visible: mode.contains(TermMode::SHOW_CURSOR) && content.display_offset == 0,
            },
            modes: map_modes(mode),
            display_offset: content.display_offset,
            title: self.title.clone(),
            hyperlinks: links.into(),
        };
        *self.cached.borrow_mut() = Some(snapshot.clone());
        Ok(snapshot)
    }

    pub fn drain_actions(&mut self, out: &mut Vec<TerminalAction>) {
        out.append(&mut self.actions);
    }

    pub fn start_selection(
        &mut self,
        kind: SelectionKind,
        point: SelectionPoint,
        side: SelectionSide,
    ) -> Result<(), TerminalError> {
        let point = self.history_point(point)?;
        self.term.selection = Some(Selection::new(
            map_selection_kind(kind),
            point,
            map_selection_side(side),
        ));
        self.bump_selection_revision()
    }

    pub fn update_selection(
        &mut self,
        point: SelectionPoint,
        side: SelectionSide,
    ) -> Result<(), TerminalError> {
        let point = self.history_point(point)?;
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, map_selection_side(side));
        }
        self.bump_selection_revision()
    }

    pub fn clear_selection(&mut self) -> Result<(), TerminalError> {
        self.term.selection = None;
        self.bump_selection_revision()
    }

    pub fn selected_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }
    pub fn input_modes(&self) -> TerminalModes {
        map_modes(*self.term.mode())
    }

    #[must_use]
    pub fn history_size(&self) -> usize {
        self.term.history_size()
    }
    pub const fn selection_revision(&self) -> u64 {
        self.selection_revision
    }

    pub fn projected_selection(&self) -> Option<ProjectedSelection> {
        let range = self.term.selection.as_ref()?.to_range(&self.term)?;
        let offset = i32::try_from(self.term.grid().display_offset()).ok()?;
        let visible = |point: Point| -> Option<[u16; 2]> {
            let line = point.line.0.checked_add(offset)?;
            if line < 0 || usize::try_from(line).ok()? >= self.size.lines() {
                return None;
            }
            Some([
                u16::try_from(point.column.0).ok()?,
                u16::try_from(line).ok()?,
            ])
        };
        Some(ProjectedSelection {
            start: visible(range.start)?,
            end: visible(range.end)?,
        })
    }

    pub fn scroll_display(&mut self, lines: i32) -> Result<(), TerminalError> {
        self.term
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TerminalError::GenerationOverflow)?;
        self.cached.get_mut().take();
        Ok(())
    }

    pub fn scroll_to_bottom(&mut self) -> Result<(), TerminalError> {
        if self.term.grid().display_offset() == 0 {
            return Ok(());
        }
        self.term
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TerminalError::GenerationOverflow)?;
        self.cached.get_mut().take();
        Ok(())
    }

    fn history_point(&self, point: SelectionPoint) -> Result<Point, TerminalError> {
        if usize::from(point.column) >= self.size.columns()
            || usize::from(point.line) >= self.size.lines()
        {
            return Err(TerminalError::SelectionPoint);
        }
        let offset = i32::try_from(self.term.grid().display_offset())
            .map_err(|_| TerminalError::SelectionPoint)?;
        Ok(Point::new(
            Line(i32::from(point.line) - offset),
            Column(usize::from(point.column)),
        ))
    }

    fn bump_selection_revision(&mut self) -> Result<(), TerminalError> {
        self.selection_revision = self
            .selection_revision
            .checked_add(1)
            .ok_or(TerminalError::GenerationOverflow)?;
        Ok(())
    }

    fn collect_events(&mut self) -> ParseAuditDelta {
        let mut audit = ParseAuditDelta::default();
        let events: Vec<_> = self.events.borrow_mut().drain(..).collect();
        for event in events {
            match event {
                Event::Title(title)
                    if let PolicyDecision::Allow(title) = validate_title(&title) =>
                {
                    self.title = Some(title.clone());
                    self.actions.push(TerminalAction::SetTitle(title));
                }
                Event::ResetTitle => {
                    self.title = None;
                }
                Event::Bell => self.actions.push(TerminalAction::Bell),
                Event::PtyWrite(text) if text.len() <= MAX_PTY_REPLY_BYTES => {
                    audit.reply_bytes = audit.reply_bytes.saturating_add(text.len());
                    self.actions
                        .push(TerminalAction::WriteToPty(text.into_bytes()));
                }
                Event::Title(_) | Event::PtyWrite(_) => {
                    audit.rejected_actions = audit.rejected_actions.saturating_add(1);
                }
                Event::ClipboardStore(..) | Event::ClipboardLoad(..) => {
                    audit.rejected_actions = audit.rejected_actions.saturating_add(1);
                    self.actions.push(TerminalAction::ClipboardRequestRejected);
                }
                Event::ColorRequest(..) | Event::TextAreaSizeRequest(..) => {
                    audit.unknown_sequences = audit.unknown_sequences.saturating_add(1);
                    self.actions.push(TerminalAction::UnsupportedSequence);
                }
                _ => {}
            }
        }
        audit
    }
}

fn map_selection_kind(kind: SelectionKind) -> SelectionType {
    match kind {
        SelectionKind::Simple => SelectionType::Simple,
        SelectionKind::Semantic => SelectionType::Semantic,
        SelectionKind::Lines => SelectionType::Lines,
    }
}
fn map_selection_side(side: SelectionSide) -> Side {
    match side {
        SelectionSide::Left => Side::Left,
        SelectionSide::Right => Side::Right,
    }
}

fn map_modes(mode: TermMode) -> TerminalModes {
    TerminalModes {
        alternate_screen: mode.contains(TermMode::ALT_SCREEN),
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        application_cursor: mode.contains(TermMode::APP_CURSOR),
        application_keypad: mode.contains(TermMode::APP_KEYPAD),
        focus_reporting: mode.contains(TermMode::FOCUS_IN_OUT),
        alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
        mouse_protocol: if mode.contains(TermMode::MOUSE_MOTION) {
            MouseProtocol::AnyEvent
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseProtocol::ButtonEvent
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseProtocol::Normal
        } else {
            MouseProtocol::None
        },
        mouse_encoding: if mode.contains(TermMode::SGR_MOUSE) {
            MouseEncoding::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            MouseEncoding::Utf8
        } else {
            MouseEncoding::Legacy
        },
    }
}

fn map_color(color: Color) -> TerminalColor {
    match color {
        Color::Named(named) => TerminalColor::Named(named as u16),
        Color::Indexed(index) => TerminalColor::Indexed(index),
        Color::Spec(rgb) => TerminalColor::Rgb(rgb.r, rgb.g, rgb.b),
    }
}
fn default_cell() -> SnapshotCell {
    SnapshotCell {
        ch: ' ',
        zerowidth: None,
        foreground: TerminalColor::Named(256),
        background: TerminalColor::Named(257),
        underline_color: None,
        flags: CellFlags::default(),
        width: CellWidth::Narrow,
        hyperlink: None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("PTY output batch is {0} bytes; the limit is 65536")]
    BatchTooLarge(usize),
    #[error("terminal generation overflow")]
    GenerationOverflow,
    #[error("terminal snapshot dimensions overflow")]
    SnapshotTooLarge,
    #[error("terminal combining character resource limit exceeded")]
    CombiningLimit,
    #[error("terminal hyperlink is too large")]
    HyperlinkTooLarge,
    #[error("terminal hyperlink table is full")]
    TooManyHyperlinks,
    #[error("selection point is outside the visible grid")]
    SelectionPoint,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_utf8_color_modes_title_and_rejects_clipboard() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(20, 4).unwrap(), 100).unwrap();
        let delta = core
            .advance(b"A\xe4\xb8\xad\x1b[38;2;1;2;3mR\x1b[?2004h\x1b]0;safe\x07\x1b]52;c;Zm9v\x07")
            .unwrap();
        let snapshot = core.snapshot().unwrap();
        assert!(snapshot.cells.iter().any(|cell| cell.ch == '\u{4e2d}'));
        assert!(
            snapshot
                .cells
                .iter()
                .any(|cell| cell.foreground == TerminalColor::Rgb(1, 2, 3))
        );
        assert!(snapshot.modes.bracketed_paste);
        assert_eq!(snapshot.title.as_deref(), Some("safe"));
        assert_eq!(delta.audit.rejected_actions, 1);
    }

    #[test]
    fn overlong_title_is_rejected_instead_of_byte_unsafe_truncation() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(4, 2).unwrap(), 0).unwrap();
        let input = format!("\x1b]0;{}\x07", "中".repeat(342));
        let delta = core.advance(input.as_bytes()).unwrap();
        assert_eq!(delta.audit.rejected_actions, 1);
        assert!(core.snapshot().unwrap().title.is_none());
    }

    #[test]
    fn unknown_sequences_are_counted_without_blocking_following_text() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 0).unwrap();
        let delta = core
            .advance(b"\x1b]999;secret\x07\x1b]998;private\x07ok")
            .unwrap();
        assert!(delta.audit.unknown_sequences >= 2);
        let visible: String = core
            .snapshot()
            .unwrap()
            .cells
            .iter()
            .map(|cell| cell.ch)
            .collect();
        assert!(visible.starts_with("ok"));
    }

    #[test]
    fn oversized_osc_is_discarded_and_parser_recovers() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(20, 4).unwrap(), 100).unwrap();
        let mut input = b"\x1b]0;".to_vec();
        input.extend(std::iter::repeat_n(
            b'x',
            alacritty_terminal::vte::MAX_OSC_RAW,
        ));
        input.push(0x07);
        let mut truncated = 0;
        for chunk in input.chunks(ByteBatch::MAX_LEN) {
            truncated += core.advance(chunk).unwrap().audit.truncated_sequences;
        }
        core.advance(b"recovered").unwrap();
        let snapshot = core.snapshot().unwrap();
        assert_eq!(truncated, 1);
        assert!(snapshot.title.is_none());
        assert_eq!(snapshot.cells[0].ch, 'r');
    }

    #[test]
    fn advance_rejects_a_batch_above_the_ingress_contract() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(2, 2).unwrap(), 0).unwrap();
        let oversized = vec![b'x'; ByteBatch::MAX_LEN + 1];
        assert!(matches!(
            core.advance(&oversized),
            Err(TerminalError::BatchTooLarge(length)) if length == oversized.len()
        ));
    }

    #[test]
    fn oversized_hyperlink_metadata_does_not_hide_cell_text() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(4, 2).unwrap(), 0).unwrap();
        let mut input = b"\x1b]8;;https://example/".to_vec();
        input.extend(std::iter::repeat_n(b'a', MAX_HYPERLINK_BYTES));
        input.extend_from_slice(b"\x1b\\X\x1b]8;;\x1b\\");
        core.advance(&input).unwrap();
        let snapshot = core.snapshot().unwrap();
        assert_eq!(snapshot.cells[0].ch, 'X');
        assert_eq!(snapshot.cells[0].hyperlink, None);
        assert!(snapshot.hyperlinks.is_empty());
    }

    #[test]
    fn excessive_combining_metadata_is_dropped_without_failing_the_terminal() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(4, 2).unwrap(), 0).unwrap();
        let input = format!("e{}", "\u{301}".repeat(MAX_ZERO_WIDTH_PER_CELL + 1));
        core.advance(input.as_bytes()).unwrap();
        let snapshot = core.snapshot().unwrap();
        assert_eq!(snapshot.cells[0].ch, 'e');
        assert!(snapshot.cells[0].zerowidth.is_none());
    }
    #[test]
    fn snapshot_reuses_cell_storage_for_unchanged_generation() {
        let core = TerminalCoreAdapter::new(GridSize::new(2, 2).unwrap(), 0).unwrap();
        let first = core.snapshot().unwrap();
        let second = core.snapshot().unwrap();
        assert!(Arc::ptr_eq(&first.cells, &second.cells));
    }

    #[test]
    fn selection_facade_extracts_text_and_projects_current_generation() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 10).unwrap();
        core.advance(b"hello").unwrap();
        core.start_selection(
            SelectionKind::Simple,
            SelectionPoint { column: 0, line: 0 },
            SelectionSide::Left,
        )
        .unwrap();
        core.update_selection(SelectionPoint { column: 4, line: 0 }, SelectionSide::Right)
            .unwrap();
        assert_eq!(core.selected_text().as_deref(), Some("hello"));
        assert_eq!(
            core.projected_selection(),
            Some(ProjectedSelection {
                start: [0, 0],
                end: [4, 0]
            })
        );
        assert_eq!(core.selection_revision(), 2);
    }

    #[test]
    fn semantic_and_line_selection_use_terminal_boundaries() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(12, 2).unwrap(), 10).unwrap();
        core.advance(b"one two\r\nsecond").unwrap();
        core.start_selection(
            SelectionKind::Semantic,
            SelectionPoint { column: 1, line: 0 },
            SelectionSide::Left,
        )
        .unwrap();
        core.update_selection(SelectionPoint { column: 1, line: 0 }, SelectionSide::Right)
            .unwrap();
        assert_eq!(core.selected_text().as_deref(), Some("one"));

        core.start_selection(
            SelectionKind::Lines,
            SelectionPoint { column: 2, line: 1 },
            SelectionSide::Left,
        )
        .unwrap();
        core.update_selection(SelectionPoint { column: 2, line: 1 }, SelectionSide::Right)
            .unwrap();
        assert_eq!(core.selected_text().as_deref(), Some("second\n"));
    }

    #[test]
    fn snapshot_preserves_indexed_color_wide_combining_cursor_and_modes() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(12, 3).unwrap(), 10).unwrap();
        core.advance(b"e\xcc\x81\xe4\xb8\xad\x1b[38;5;196mX\x1b[2;4H\x1b[?1h\x1b[?1000h")
            .unwrap();
        let snapshot = core.snapshot().unwrap();
        assert!(snapshot.cells.iter().any(|cell| {
            cell.ch == 'e' && cell.zerowidth.as_deref() == Some(['\u{301}'].as_slice())
        }));
        assert!(
            snapshot
                .cells
                .iter()
                .any(|cell| cell.ch == '\u{4e2d}' && cell.width == CellWidth::Wide)
        );
        assert!(
            snapshot
                .cells
                .iter()
                .any(|cell| cell.ch == 'X' && cell.foreground == TerminalColor::Indexed(196))
        );
        assert_eq!((snapshot.cursor.column, snapshot.cursor.line), (3, 1));
        assert!(
            snapshot.modes.application_cursor
                && snapshot.modes.mouse_protocol == MouseProtocol::Normal
        );
    }

    #[test]
    fn alternate_screen_restores_main_screen() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 10).unwrap();
        core.advance(b"MAIN\x1b[?1049hALT").unwrap();
        assert!(core.snapshot().unwrap().modes.alternate_screen);
        core.advance(b"\x1b[?1049l").unwrap();
        let snapshot = core.snapshot().unwrap();
        assert!(!snapshot.modes.alternate_screen);
        assert!(
            snapshot
                .cells
                .iter()
                .map(|cell| cell.ch)
                .collect::<String>()
                .contains("MAIN")
        );
    }

    #[test]
    fn user_input_can_restore_scrollback_to_bottom() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 10).unwrap();
        core.advance(b"one\r\ntwo\r\nthree").unwrap();
        core.scroll_display(1).unwrap();
        assert_eq!(core.snapshot().unwrap().display_offset, 1);
        core.scroll_to_bottom().unwrap();
        assert_eq!(core.snapshot().unwrap().display_offset, 0);
    }

    #[test]
    fn cursor_is_hidden_while_viewing_scrollback() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 10).unwrap();
        core.advance(b"one\r\ntwo\r\nthree").unwrap();
        assert!(core.snapshot().unwrap().cursor.visible);

        core.scroll_display(1).unwrap();
        let snapshot = core.snapshot().unwrap();
        assert_eq!(snapshot.display_offset, 1);
        assert!(!snapshot.cursor.visible);

        core.scroll_to_bottom().unwrap();
        assert!(core.snapshot().unwrap().cursor.visible);
    }

    #[test]
    fn parser_chunking_does_not_change_final_snapshot() {
        let fixture = b"A\xe4\xb8\xad\x1b[38;2;4;5;6mRGB\x1b[0m\x1b]0;chunked\x07";
        let mut whole = TerminalCoreAdapter::new(GridSize::new(16, 3).unwrap(), 10).unwrap();
        whole.advance(fixture).unwrap();
        let mut chunked = TerminalCoreAdapter::new(GridSize::new(16, 3).unwrap(), 10).unwrap();
        for byte in fixture {
            chunked.advance(std::slice::from_ref(byte)).unwrap();
        }
        let whole = whole.snapshot().unwrap();
        let chunked = chunked.snapshot().unwrap();
        assert_eq!(whole.cells, chunked.cells);
        assert_eq!(whole.cursor, chunked.cursor);
        assert_eq!(whole.modes, chunked.modes);
        assert_eq!(whole.title, chunked.title);
    }

    #[test]
    fn deterministic_random_chunking_preserves_state_and_audit() {
        let fixture = b"A\xe4\xb8\xad\x1b[38;2;4;5;6mRGB\x1b[0m\x1b]0;chunked\x07\x1b]8;;https://example.test\x1b\\X\x1b]8;;\x1b\\\x1b]52;c;Zm9v\x07\x1b]999;private\x07Z";
        let mut whole = TerminalCoreAdapter::new(GridSize::new(32, 3).unwrap(), 10).unwrap();
        let expected_audit = whole.advance(fixture).unwrap().audit;
        let expected = whole.snapshot().unwrap();

        for seed in [1_u64, 0x5eed, 0xdead_beef] {
            let mut state = seed;
            let mut offset = 0;
            let mut audit = ParseAuditDelta::default();
            let mut chunked = TerminalCoreAdapter::new(GridSize::new(32, 3).unwrap(), 10).unwrap();
            while offset < fixture.len() {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let length = (usize::try_from(state >> 32).unwrap_or(1) % 11 + 1)
                    .min(fixture.len() - offset);
                let delta = chunked.advance(&fixture[offset..offset + length]).unwrap();
                audit.unknown_sequences = audit
                    .unknown_sequences
                    .saturating_add(delta.audit.unknown_sequences);
                audit.rejected_actions = audit
                    .rejected_actions
                    .saturating_add(delta.audit.rejected_actions);
                audit.truncated_sequences = audit
                    .truncated_sequences
                    .saturating_add(delta.audit.truncated_sequences);
                audit.reply_bytes = audit.reply_bytes.saturating_add(delta.audit.reply_bytes);
                offset += length;
            }
            let actual = chunked.snapshot().unwrap();
            assert_eq!(actual.cells, expected.cells, "seed {seed}");
            assert_eq!(actual.cursor, expected.cursor, "seed {seed}");
            assert_eq!(actual.modes, expected.modes, "seed {seed}");
            assert_eq!(actual.title, expected.title, "seed {seed}");
            assert_eq!(audit, expected_audit, "seed {seed}");
        }
    }

    #[test]
    fn scrollback_storage_stops_at_the_configured_limit() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(4, 2).unwrap(), 10).unwrap();
        core.advance("line\r\n".repeat(100).as_bytes()).unwrap();
        assert_eq!(core.history_size(), 10);
    }
}

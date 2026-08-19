use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc, time::Instant};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener, WindowSize},
    grid::Dimensions,
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::{Config, TermMode, cell::Flags, test::TermSize},
    vte::ansi::{self, Color, NamedColor, Rgb},
};

use super::protocol::{KeyboardProtocolTracker, ProtocolAudit};
use super::search::{
    CompiledLiteral, CompiledRegex, RegexScanCursor, RegexScanStep, SearchAnchor, SearchBudget,
    SearchContentId, SearchError, SearchMatch, SearchProjection, SearchScanCursor, SearchScanStep,
};
use super::snapshot::{
    CellFlags, CellWidth, CursorBlink, CursorShape, CursorSnapshot, FrameSnapshot, GridSize,
    MouseEncoding, MouseProtocol, ProjectedSelection, SearchBuffer, SelectionKind, SelectionPoint,
    SelectionSide, SnapshotCell, SnapshotHyperlink, TerminalColor, TerminalModes, UnderlineStyle,
};
use crate::{
    app::event::ByteBatch,
    security::{
        MAX_HYPERLINK_BYTES, MAX_HYPERLINKS, MAX_PTY_REPLY_BYTES, MAX_ZERO_WIDTH_PER_CELL,
        MAX_ZERO_WIDTH_TOTAL, PolicyDecision, validate_title,
    },
};

const ADDITIONAL_SEMANTIC_ESCAPE_CHARS: &str = ";，。：；";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalAction {
    SetTitle(Arc<str>),
    ResetTitle,
    Bell,
    WriteToPty(Vec<u8>),
    Query(TerminalQuery),
    ClipboardRequestRejected,
    UnsupportedSequence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefaultColorSlot {
    Foreground,
    Background,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryTerminator {
    Bell,
    StringTerminator,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalQuery {
    DefaultColor {
        slot: DefaultColorSlot,
        terminator: QueryTerminator,
    },
    TextAreaPixels,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalCoreConfig {
    pub history_lines: usize,
    pub default_cursor_shape: CursorShape,
}

impl From<usize> for TerminalCoreConfig {
    fn from(history_lines: usize) -> Self {
        Self {
            history_lines,
            default_cursor_shape: CursorShape::Block,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingSync {
    pub epoch: u64,
    pub bytes: usize,
    pub deadline: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncFlushReason {
    Timeout,
    SessionEnd,
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
    pub sync_forced_commits: u32,
    pub sync_timeouts: u32,
    pub query_replies: u32,
    pub query_rejected: u32,
    pub display_state_fallbacks: u32,
    pub keyboard_protocol_changes: u32,
    pub keyboard_queries: u32,
    pub keyboard_unknown_flags: u32,
    pub keyboard_stack_overflow: u32,
}

#[derive(Default)]
struct ListenerState {
    events: Vec<Event>,
    bell_pending: bool,
}

#[derive(Clone, Default)]
struct Listener(Rc<RefCell<ListenerState>>);
impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut state = self.0.borrow_mut();
        if matches!(event, Event::Bell) {
            state.bell_pending = true;
        } else {
            state.events.push(event);
        }
    }
}

pub struct TerminalCoreAdapter {
    term: Term<Listener>,
    parser: ansi::Processor,
    events: Rc<RefCell<ListenerState>>,
    actions: Vec<TerminalAction>,
    generation: u64,
    content_revision: u64,
    size: GridSize,
    title: Option<Arc<str>>,
    cached: RefCell<Option<FrameSnapshot>>,
    selection_revision: u64,
    sync_epoch: u64,
    keyboard_protocol: KeyboardProtocolTracker,
    _main_thread: Rc<()>,
}

#[allow(clippy::missing_errors_doc)]
impl TerminalCoreAdapter {
    pub fn new(
        size: GridSize,
        config: impl Into<TerminalCoreConfig>,
    ) -> Result<Self, TerminalError> {
        let config = config.into();
        let listener = Listener::default();
        let events = Rc::clone(&listener.0);
        let config = Config {
            scrolling_history: config.history_lines.min(100_000),
            default_cursor_style: ansi::CursorStyle {
                shape: map_cursor_shape_to_ansi(config.default_cursor_shape),
                blinking: false,
            },
            semantic_escape_chars: format!(
                "{}{}",
                alacritty_terminal::term::SEMANTIC_ESCAPE_CHARS,
                ADDITIONAL_SEMANTIC_ESCAPE_CHARS
            ),
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
            content_revision: 0,
            size,
            title: None,
            cached: RefCell::new(None),
            selection_revision: 0,
            sync_epoch: 0,
            keyboard_protocol: KeyboardProtocolTracker::default(),
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
        let (keyboard_replies, keyboard_audit) = self.keyboard_protocol.advance(bytes);
        let was_pending = self.parser.sync_timeout().sync_timeout().is_some();
        let mut start = 0;
        for (offset, reply) in keyboard_replies {
            self.parser.advance(&mut self.term, &bytes[start..=offset]);
            self.queue_keyboard_reply(reply);
            start = offset.saturating_add(1);
        }
        self.parser.advance(&mut self.term, &bytes[start..]);
        let commits = self.parser.take_sync_commit_delta();
        let is_pending = self.parser.sync_timeout().sync_timeout().is_some();
        if is_pending && (!was_pending || commits.explicit != 0 || commits.capacity != 0) {
            self.sync_epoch = self
                .sync_epoch
                .checked_add(1)
                .ok_or(TerminalError::GenerationOverflow)?;
        }
        let parser_audit = self.parser.take_audit_delta();
        let dirty = !is_pending || commits.explicit != 0 || commits.capacity != 0;
        if !dirty {
            let event_audit = self.collect_events();
            return Ok(TerminalDelta {
                dirty: false,
                actions: self.actions.len(),
                audit: merge_audit(event_audit, parser_audit, commits.capacity, keyboard_audit),
            });
        }
        self.bump_content_revision()?;
        self.cached.get_mut().take();
        let event_audit = self.collect_events();
        Ok(TerminalDelta {
            dirty,
            actions: self.actions.len(),
            audit: ParseAuditDelta {
                unknown_sequences: event_audit
                    .unknown_sequences
                    .saturating_add(parser_audit.unknown_sequences),
                rejected_actions: event_audit
                    .rejected_actions
                    .saturating_add(parser_audit.rejected_actions)
                    .saturating_add(keyboard_audit.rejected),
                truncated_sequences: parser_audit.truncated_sequences,
                reply_bytes: event_audit.reply_bytes,
                sync_forced_commits: commits.capacity,
                sync_timeouts: 0,
                query_replies: event_audit.query_replies,
                query_rejected: event_audit.query_rejected,
                display_state_fallbacks: 0,
                keyboard_protocol_changes: keyboard_audit.changes,
                keyboard_queries: keyboard_audit.queries,
                keyboard_unknown_flags: keyboard_audit.unknown_flags,
                keyboard_stack_overflow: keyboard_audit.stack_overflow,
            },
        })
    }

    pub fn reset_keyboard_protocol(&mut self) {
        self.keyboard_protocol.reset();
    }

    #[must_use]
    pub fn pending_sync(&self) -> Option<PendingSync> {
        Some(PendingSync {
            epoch: self.sync_epoch,
            bytes: self.parser.sync_bytes_count(),
            deadline: self.parser.sync_timeout().sync_timeout()?,
        })
    }

    pub fn flush_synchronized_update(
        &mut self,
        epoch: u64,
        reason: SyncFlushReason,
    ) -> Result<TerminalDelta, TerminalError> {
        if self
            .pending_sync()
            .is_none_or(|pending| pending.epoch != epoch)
        {
            return Ok(TerminalDelta::default());
        }
        self.parser.stop_sync(&mut self.term);
        self.parser.take_sync_commit_delta();
        let parser_audit = self.parser.take_audit_delta();
        self.bump_content_revision()?;
        self.cached.get_mut().take();
        let event_audit = self.collect_events();
        let mut audit = merge_audit(event_audit, parser_audit, 0, ProtocolAudit::default());
        audit.sync_timeouts = u32::from(matches!(reason, SyncFlushReason::Timeout));
        Ok(TerminalDelta {
            dirty: true,
            actions: self.actions.len(),
            audit,
        })
    }

    pub fn discard_synchronized_update(&mut self, epoch: u64) -> bool {
        if self
            .pending_sync()
            .is_none_or(|pending| pending.epoch != epoch)
        {
            return false;
        }
        self.parser.discard_sync();
        true
    }

    pub fn resize(&mut self, size: GridSize) -> Result<TerminalDelta, TerminalError> {
        if size == self.size {
            return Ok(TerminalDelta::default());
        }
        self.term
            .resize(TermSize::new(size.columns(), size.lines()));
        self.size = size;
        self.bump_content_revision()?;
        self.cached.get_mut().take();
        Ok(TerminalDelta {
            dirty: true,
            actions: 0,
            audit: ParseAuditDelta::default(),
        })
    }

    #[must_use]
    pub const fn size(&self) -> GridSize {
        self.size
    }

    #[must_use]
    pub fn search_content_id(&self) -> SearchContentId {
        SearchContentId::new(
            self.content_revision,
            self.size,
            if self.term.mode().contains(TermMode::ALT_SCREEN) {
                SearchBuffer::Alternate
            } else {
                SearchBuffer::Normal
            },
        )
    }

    pub fn compile_literal_search(&self, query: &str) -> Result<CompiledLiteral, SearchError> {
        super::search::compile(query)
    }

    pub fn compile_regex_search(&self, query: &str) -> Result<CompiledRegex, SearchError> {
        super::search::compile_regex(query)
    }

    #[must_use]
    pub fn search_scan_cursor(&self) -> SearchScanCursor {
        SearchScanCursor {
            content: self.search_content_id(),
            line: self.term.topmost_line().0,
            column: 0,
            scalar_offset: 0,
            progress: 0,
            recent: std::collections::VecDeque::new(),
        }
    }

    #[must_use]
    pub fn regex_scan_cursor(&self) -> RegexScanCursor {
        RegexScanCursor {
            content: self.search_content_id(),
            line: self.term.topmost_line().0,
            column: 0,
            scalar_offset: 0,
            text: String::new(),
            anchors: Vec::new(),
            byte_offsets: Vec::new(),
            matching: false,
            match_offset: 0,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn scan_regex_slice(
        &self,
        compiled: &CompiledRegex,
        mut cursor: RegexScanCursor,
        budget: SearchBudget,
    ) -> Result<RegexScanStep, SearchError> {
        let content = self.search_content_id();
        if cursor.content != content {
            return Err(SearchError::StaleContent);
        }
        let max_rows = budget.max_rows.min(super::search::MAX_SEARCH_SLICE_ROWS);
        let max_matches = budget.max_matches.min(super::search::MAX_SEARCH_MATCHES);
        let last_line =
            i32::try_from(self.size.lines()).map_err(|_| SearchError::InvalidCoordinate)? - 1;
        let columns = self.size.columns();
        let mut matches = Vec::new();
        let mut scanned_rows = 0;
        let mut scalars_since_clock = 0;

        loop {
            if cursor.matching {
                collect_regex_matches(compiled, &mut cursor, &mut matches, max_matches)?;
                if matches.len() == max_matches {
                    return Ok(RegexScanStep {
                        content,
                        matches,
                        next: Some(cursor),
                        scanned_rows,
                    });
                }
                cursor.text.clear();
                cursor.anchors.clear();
                cursor.byte_offsets.clear();
                cursor.matching = false;
                cursor.match_offset = 0;
            }
            if cursor.line > last_line || scanned_rows == max_rows {
                break;
            }
            let cell = &self.term.grid()[Point::new(Line(cursor.line), Column(cursor.column))];
            let is_spacer = cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
            let zero_width = cell
                .zerowidth()
                .filter(|value| !value.is_empty() && value.len() <= MAX_ZERO_WIDTH_PER_CELL);
            let scalar_count = if is_spacer {
                0
            } else {
                1 + zero_width.map_or(0, <[char]>::len)
            };
            while cursor.scalar_offset < scalar_count {
                let value = if cursor.scalar_offset == 0 {
                    if cell.c == '\t' { ' ' } else { cell.c }
                } else {
                    *zero_width
                        .and_then(|values| values.get(cursor.scalar_offset - 1))
                        .ok_or(SearchError::InvalidCoordinate)?
                };
                let next_bytes = cursor
                    .text
                    .len()
                    .checked_add(value.len_utf8())
                    .ok_or(SearchError::RegexLineTooLong)?;
                if next_bytes > super::search::MAX_REGEX_LOGICAL_LINE_BYTES {
                    return Err(SearchError::RegexLineTooLong);
                }
                cursor
                    .text
                    .try_reserve(value.len_utf8())
                    .map_err(|_| SearchError::Allocation)?;
                cursor
                    .anchors
                    .try_reserve(1)
                    .map_err(|_| SearchError::Allocation)?;
                cursor
                    .byte_offsets
                    .try_reserve(1)
                    .map_err(|_| SearchError::Allocation)?;
                cursor.byte_offsets.push(cursor.text.len());
                cursor.anchors.push(SearchAnchor {
                    history_line: cursor.line,
                    column: u16::try_from(cursor.column)
                        .map_err(|_| SearchError::InvalidCoordinate)?,
                    scalar_offset: u8::try_from(cursor.scalar_offset)
                        .map_err(|_| SearchError::InvalidCoordinate)?,
                });
                cursor.text.push(value);
                cursor.scalar_offset += 1;
                scalars_since_clock += 1;
                if scalars_since_clock == 64 {
                    scalars_since_clock = 0;
                    if (budget.clock)() >= budget.deadline {
                        return Ok(RegexScanStep {
                            content,
                            matches,
                            next: Some(cursor),
                            scanned_rows,
                        });
                    }
                }
            }
            cursor.scalar_offset = 0;
            cursor.column += 1;
            if cursor.column == columns {
                let wrapped = cell.flags.contains(Flags::WRAPLINE);
                cursor.column = 0;
                cursor.line += 1;
                scanned_rows += 1;
                if !wrapped || cursor.line > last_line {
                    cursor.matching = true;
                }
                if (budget.clock)() >= budget.deadline {
                    break;
                }
            }
        }
        let next = (cursor.line <= last_line || cursor.matching).then_some(cursor);
        Ok(RegexScanStep {
            content,
            matches,
            next,
            scanned_rows,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub fn scan_search_slice(
        &self,
        compiled: &CompiledLiteral,
        mut cursor: SearchScanCursor,
        budget: SearchBudget,
    ) -> Result<SearchScanStep, SearchError> {
        let content = self.search_content_id();
        if cursor.content != content {
            return Err(SearchError::StaleContent);
        }
        let max_rows = budget.max_rows.min(super::search::MAX_SEARCH_SLICE_ROWS);
        let max_matches = budget.max_matches.min(super::search::MAX_SEARCH_MATCHES);
        let last_line =
            i32::try_from(self.size.lines()).map_err(|_| SearchError::InvalidCoordinate)? - 1;
        let columns = self.size.columns();
        let mut matches = Vec::new();
        let mut scanned_rows = 0;
        let mut scalars_since_clock = 0;

        while cursor.line <= last_line && scanned_rows < max_rows && matches.len() < max_matches {
            let cell = &self.term.grid()[Point::new(Line(cursor.line), Column(cursor.column))];
            let is_spacer = cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
            let zero_width = cell
                .zerowidth()
                .filter(|value| !value.is_empty() && value.len() <= MAX_ZERO_WIDTH_PER_CELL);
            let scalar_count = if is_spacer {
                0
            } else {
                1 + zero_width.map_or(0, <[char]>::len)
            };
            while cursor.scalar_offset < scalar_count {
                let value = if cursor.scalar_offset == 0 {
                    if cell.c == '\t' { ' ' } else { cell.c }
                } else {
                    *zero_width
                        .and_then(|values| values.get(cursor.scalar_offset - 1))
                        .ok_or(SearchError::InvalidCoordinate)?
                };
                let anchor = SearchAnchor {
                    history_line: cursor.line,
                    column: u16::try_from(cursor.column)
                        .map_err(|_| SearchError::InvalidCoordinate)?,
                    scalar_offset: u8::try_from(cursor.scalar_offset)
                        .map_err(|_| SearchError::InvalidCoordinate)?,
                };
                while cursor.progress > 0 && value != compiled.pattern[cursor.progress] {
                    cursor.progress = compiled.prefix[cursor.progress - 1];
                }
                if value == compiled.pattern[cursor.progress] {
                    cursor.progress += 1;
                }
                cursor.recent.push_back(anchor);
                while cursor.recent.len() > compiled.pattern.len() {
                    cursor.recent.pop_front();
                }
                cursor.scalar_offset += 1;
                scalars_since_clock += 1;
                if cursor.progress == compiled.pattern.len() {
                    if let Some(start) = cursor.recent.front().copied() {
                        matches.push(SearchMatch { start, end: anchor });
                    }
                    cursor.progress = compiled.prefix[cursor.progress - 1];
                }
                if scalars_since_clock == 64 {
                    scalars_since_clock = 0;
                    if (budget.clock)() >= budget.deadline {
                        return Ok(SearchScanStep {
                            content,
                            matches,
                            next: Some(cursor),
                            scanned_rows,
                        });
                    }
                }
                if matches.len() == max_matches {
                    return Ok(SearchScanStep {
                        content,
                        matches,
                        next: Some(cursor),
                        scanned_rows,
                    });
                }
            }
            cursor.scalar_offset = 0;
            cursor.column += 1;
            if cursor.column == columns {
                let wrapped = cell.flags.contains(Flags::WRAPLINE);
                cursor.column = 0;
                cursor.line += 1;
                scanned_rows += 1;
                if !wrapped {
                    cursor.progress = 0;
                    cursor.recent.clear();
                }
                if (budget.clock)() >= budget.deadline {
                    break;
                }
            }
        }
        let next = (cursor.line <= last_line).then_some(cursor);
        Ok(SearchScanStep {
            content,
            matches,
            next,
            scanned_rows,
        })
    }

    pub fn project_search_match(
        &self,
        value: SearchMatch,
    ) -> Result<SearchProjection, SearchError> {
        let offset = i32::try_from(self.term.grid().display_offset())
            .map_err(|_| SearchError::InvalidCoordinate)?;
        let project = |anchor: SearchAnchor| -> Result<[u16; 2], SearchError> {
            if usize::from(anchor.column) >= self.size.columns() {
                return Err(SearchError::InvalidCoordinate);
            }
            let line = anchor
                .history_line
                .checked_add(offset)
                .ok_or(SearchError::InvalidCoordinate)?;
            if line < 0
                || usize::try_from(line)
                    .ok()
                    .is_none_or(|line| line >= self.size.lines())
            {
                return Err(SearchError::InvalidCoordinate);
            }
            Ok([
                anchor.column,
                u16::try_from(line).map_err(|_| SearchError::InvalidCoordinate)?,
            ])
        };
        Ok(SearchProjection {
            start: project(value.start)?,
            end: project(value.end)?,
        })
    }

    pub fn search_display_offset(&self, value: SearchMatch) -> Result<usize, SearchError> {
        if value.start > value.end {
            return Err(SearchError::InvalidCoordinate);
        }
        let viewport_top = -i32::try_from(self.term.grid().display_offset())
            .map_err(|_| SearchError::InvalidCoordinate)?;
        let viewport_bottom = viewport_top
            .checked_add(
                i32::try_from(self.size.lines()).map_err(|_| SearchError::InvalidCoordinate)? - 1,
            )
            .ok_or(SearchError::InvalidCoordinate)?;
        let target_top = if value.start.history_line < viewport_top {
            value.start.history_line
        } else if value.end.history_line > viewport_bottom {
            value.end.history_line
                - i32::try_from(self.size.lines()).map_err(|_| SearchError::InvalidCoordinate)?
                + 1
        } else {
            viewport_top
        };
        usize::try_from(target_top.saturating_neg())
            .map(|offset| offset.min(self.term.history_size()))
            .map_err(|_| SearchError::InvalidCoordinate)
    }

    #[allow(clippy::too_many_lines)]
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
            // Alacritty stores HT in the originating cell as an internal marker. It is not a
            // printable glyph; exposing it to the renderer produces a missing-glyph box.
            let ch = if cell.c == '\t' { ' ' } else { cell.c };
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
                ch,
                zerowidth: zerowidth.map(Arc::from),
                foreground: map_color(cell.fg),
                background: map_color(cell.bg),
                underline_color: cell.underline_color().map(map_color),
                underline_style: map_underline_style(cell.flags),
                flags: CellFlags {
                    bold: cell.flags.contains(Flags::BOLD),
                    dim: cell.flags.contains(Flags::DIM),
                    italic: cell.flags.contains(Flags::ITALIC),
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
        let cursor_style = self.term.cursor_style();
        let snapshot = FrameSnapshot {
            generation: self.generation,
            content_revision: self.content_revision,
            active_buffer: if mode.contains(TermMode::ALT_SCREEN) {
                SearchBuffer::Alternate
            } else {
                SearchBuffer::Normal
            },
            grid: self.size,
            cells: cells.into(),
            cursor: CursorSnapshot {
                column: u16::try_from(cursor.point.column.0).unwrap_or(u16::MAX),
                line: u16::try_from(cursor.point.line.0.max(0)).unwrap_or(u16::MAX),
                visible: mode.contains(TermMode::SHOW_CURSOR) && content.display_offset == 0,
                shape: map_cursor_shape(cursor_style.shape),
                blink: if cursor_style.blinking {
                    CursorBlink::Blinking
                } else {
                    CursorBlink::Steady
                },
            },
            modes: map_modes(mode, self.keyboard_protocol.state()),
            display_offset: content.display_offset,
            history_size: self.term.history_size(),
            title: self.title.clone(),
            hyperlinks: links.into(),
        };
        *self.cached.borrow_mut() = Some(snapshot.clone());
        Ok(snapshot)
    }

    pub fn drain_actions(&mut self, out: &mut Vec<TerminalAction>) {
        out.append(&mut self.actions);
    }

    fn bump_content_revision(&mut self) -> Result<(), TerminalError> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(TerminalError::GenerationOverflow)?;
        let content_revision = self
            .content_revision
            .checked_add(1)
            .ok_or(TerminalError::GenerationOverflow)?;
        self.generation = generation;
        self.content_revision = content_revision;
        Ok(())
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
        map_modes(*self.term.mode(), self.keyboard_protocol.state())
    }

    fn queue_keyboard_reply(&mut self, reply: Vec<u8>) {
        if let Ok(reply) = String::from_utf8(reply) {
            self.events.borrow_mut().events.push(Event::PtyWrite(reply));
        }
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

    pub fn scroll_to_display_offset(&mut self, offset: usize) -> Result<(), TerminalError> {
        let target = offset.min(self.term.history_size());
        let current = self.term.grid().display_offset();
        if target == current {
            return Ok(());
        }
        let delta = i64::try_from(target)
            .ok()
            .and_then(|target| i64::try_from(current).ok().map(|current| target - current))
            .and_then(|delta| i32::try_from(delta).ok())
            .ok_or(TerminalError::GenerationOverflow)?;
        self.scroll_display(delta)
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
        let (events, bell_pending) = {
            let mut state = self.events.borrow_mut();
            (
                std::mem::take(&mut state.events),
                std::mem::take(&mut state.bell_pending),
            )
        };
        if bell_pending {
            self.actions.push(TerminalAction::Bell);
        }
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
                    self.actions.push(TerminalAction::ResetTitle);
                }
                Event::Bell => unreachable!("BEL is coalesced by the event listener"),
                Event::PtyWrite(text)
                    if text.len() <= MAX_PTY_REPLY_BYTES
                        && audit.reply_bytes.saturating_add(text.len()) <= MAX_PTY_REPLY_BYTES =>
                {
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
                Event::ColorRequest(index, formatter) => {
                    if let Some(query) = map_color_query(index, &formatter) {
                        self.actions.push(TerminalAction::Query(query));
                    } else {
                        audit.query_rejected = audit.query_rejected.saturating_add(1);
                    }
                }
                Event::TextAreaSizeRequest(formatter) => {
                    if validate_text_area_formatter(&formatter) {
                        self.actions
                            .push(TerminalAction::Query(TerminalQuery::TextAreaPixels));
                    } else {
                        audit.query_rejected = audit.query_rejected.saturating_add(1);
                    }
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

fn map_modes(mode: TermMode, keyboard: super::KeyboardProtocolState) -> TerminalModes {
    TerminalModes {
        alternate_screen: mode.contains(TermMode::ALT_SCREEN),
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        application_cursor: mode.contains(TermMode::APP_CURSOR),
        application_keypad: mode.contains(TermMode::APP_KEYPAD),
        keyboard,
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

fn map_cursor_shape_to_ansi(shape: CursorShape) -> ansi::CursorShape {
    match shape {
        CursorShape::Block => ansi::CursorShape::Block,
        CursorShape::Beam => ansi::CursorShape::Beam,
        CursorShape::Underline => ansi::CursorShape::Underline,
    }
}

fn map_cursor_shape(shape: ansi::CursorShape) -> CursorShape {
    match shape {
        ansi::CursorShape::Beam => CursorShape::Beam,
        ansi::CursorShape::Underline => CursorShape::Underline,
        ansi::CursorShape::Block | ansi::CursorShape::HollowBlock | ansi::CursorShape::Hidden => {
            CursorShape::Block
        }
    }
}

fn map_underline_style(flags: Flags) -> UnderlineStyle {
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        UnderlineStyle::Double
    } else if flags.contains(Flags::UNDERCURL) {
        UnderlineStyle::Curly
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        UnderlineStyle::Dotted
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        UnderlineStyle::Dashed
    } else if flags.contains(Flags::UNDERLINE) {
        UnderlineStyle::Single
    } else {
        UnderlineStyle::None
    }
}

fn map_color_query(
    index: usize,
    formatter: &Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>,
) -> Option<TerminalQuery> {
    let slot = match index {
        value if value == NamedColor::Foreground as usize => DefaultColorSlot::Foreground,
        value if value == NamedColor::Background as usize => DefaultColorSlot::Background,
        _ => return None,
    };
    let code = match slot {
        DefaultColorSlot::Foreground => 10,
        DefaultColorSlot::Background => 11,
    };
    let probe_reply = formatter(Rgb { r: 1, g: 2, b: 3 });
    let terminator = if probe_reply == format!("\x1b]{code};rgb:0101/0202/0303\x07") {
        QueryTerminator::Bell
    } else if probe_reply == format!("\x1b]{code};rgb:0101/0202/0303\x1b\\") {
        QueryTerminator::StringTerminator
    } else {
        return None;
    };
    Some(TerminalQuery::DefaultColor { slot, terminator })
}

fn validate_text_area_formatter(
    formatter: &Arc<dyn Fn(WindowSize) -> String + Sync + Send + 'static>,
) -> bool {
    formatter(WindowSize {
        num_lines: 2,
        num_cols: 3,
        cell_width: 5,
        cell_height: 7,
    }) == "\x1b[4;14;15t"
}

fn merge_audit(
    event: ParseAuditDelta,
    parser: alacritty_terminal::vte::ParseAuditDelta,
    forced_commits: u32,
    keyboard: ProtocolAudit,
) -> ParseAuditDelta {
    ParseAuditDelta {
        unknown_sequences: event
            .unknown_sequences
            .saturating_add(parser.unknown_sequences),
        rejected_actions: event
            .rejected_actions
            .saturating_add(parser.rejected_actions)
            .saturating_add(keyboard.rejected),
        truncated_sequences: parser.truncated_sequences,
        reply_bytes: event.reply_bytes,
        sync_forced_commits: forced_commits,
        sync_timeouts: event.sync_timeouts,
        query_replies: event.query_replies,
        query_rejected: event.query_rejected,
        display_state_fallbacks: event.display_state_fallbacks,
        keyboard_protocol_changes: event
            .keyboard_protocol_changes
            .saturating_add(keyboard.changes),
        keyboard_queries: event.keyboard_queries.saturating_add(keyboard.queries),
        keyboard_unknown_flags: event
            .keyboard_unknown_flags
            .saturating_add(keyboard.unknown_flags),
        keyboard_stack_overflow: event
            .keyboard_stack_overflow
            .saturating_add(keyboard.stack_overflow),
    }
}
fn default_cell() -> SnapshotCell {
    SnapshotCell {
        ch: ' ',
        zerowidth: None,
        foreground: TerminalColor::Named(256),
        background: TerminalColor::Named(257),
        underline_color: None,
        underline_style: UnderlineStyle::None,
        flags: CellFlags::default(),
        width: CellWidth::Narrow,
        hyperlink: None,
    }
}

fn collect_regex_matches(
    compiled: &CompiledRegex,
    cursor: &mut RegexScanCursor,
    matches: &mut Vec<SearchMatch>,
    max_matches: usize,
) -> Result<(), SearchError> {
    while cursor.match_offset <= cursor.text.len() && matches.len() < max_matches {
        let input =
            regex_automata::Input::new(&cursor.text).span(cursor.match_offset..cursor.text.len());
        let Some(found) = compiled.regex.find(input) else {
            cursor.matching = false;
            return Ok(());
        };
        if found.start() == found.end() {
            let Some(next) = cursor.text[found.end()..]
                .chars()
                .next()
                .map(|value| found.end() + value.len_utf8())
            else {
                cursor.matching = false;
                return Ok(());
            };
            cursor.match_offset = next;
            continue;
        }
        let start_index = cursor
            .byte_offsets
            .binary_search(&found.start())
            .map_err(|_| SearchError::InvalidCoordinate)?;
        let end_index = cursor
            .byte_offsets
            .partition_point(|offset| *offset < found.end())
            .checked_sub(1)
            .ok_or(SearchError::InvalidCoordinate)?;
        let start = *cursor
            .anchors
            .get(start_index)
            .ok_or(SearchError::InvalidCoordinate)?;
        let end = *cursor
            .anchors
            .get(end_index)
            .ok_or(SearchError::InvalidCoordinate)?;
        matches.push(SearchMatch { start, end });
        cursor.match_offset = found.end();
    }
    Ok(())
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
    fn horizontal_tab_advances_without_exposing_a_control_glyph() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(16, 2).unwrap(), 0).unwrap();
        core.advance(b"\tX").unwrap();

        let snapshot = core.snapshot().unwrap();
        assert_eq!(snapshot.cells[8].ch, 'X');
        assert!(!snapshot.cells.iter().any(|cell| cell.ch == '\t'));
        assert_eq!(snapshot.cursor.column, 9);
    }

    #[test]
    fn standard_device_queries_emit_bounded_xterm_replies() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(20, 4).unwrap(), 100).unwrap();
        let delta = core
            .advance(b"\x1b[c\x1b[>c\x1b[5n\x1b[2;3H\x1b[6n\x1b[?1$p\x1b[4$p\x1b[?9999$p\x1b[99n")
            .unwrap();
        let mut actions = Vec::new();
        core.drain_actions(&mut actions);
        let replies: Vec<Vec<u8>> = actions
            .into_iter()
            .filter_map(|action| match action {
                TerminalAction::WriteToPty(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        assert_eq!(
            replies,
            [
                b"\x1b[?6c".to_vec(),
                b"\x1b[>0;2501;1c".to_vec(),
                b"\x1b[0n".to_vec(),
                b"\x1b[2;3R".to_vec(),
                b"\x1b[?1;2$y".to_vec(),
                b"\x1b[4;2$y".to_vec(),
                b"\x1b[?9999;0$y".to_vec(),
            ]
        );
        assert_eq!(delta.audit.unknown_sequences, 1);
        assert_eq!(
            delta.audit.reply_bytes,
            replies.iter().map(Vec::len).sum::<usize>()
        );
        assert!(delta.audit.reply_bytes <= MAX_PTY_REPLY_BYTES);
    }

    #[test]
    fn keyboard_protocol_state_query_and_screen_lifecycle_form_a_closed_loop() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(20, 4).unwrap(), 0).unwrap();
        let delta = core
            .advance(b"\x1b[=3;1u\x1b[>4;2m\x1b[?u\x1b[?4m")
            .unwrap();
        assert_eq!(core.input_modes().keyboard.kitty.bits(), 3);
        assert_eq!(
            core.input_modes().keyboard.modify_other_keys,
            super::super::ModifyOtherKeysLevel::All
        );
        let mut actions = Vec::new();
        core.drain_actions(&mut actions);
        let replies: Vec<_> = actions
            .into_iter()
            .filter_map(|action| match action {
                TerminalAction::WriteToPty(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        assert_eq!(replies, [b"\x1b[?3u".to_vec(), b"\x1b[>4;2m".to_vec()]);
        assert_eq!(delta.audit.keyboard_queries, 2);
        assert_eq!(delta.audit.keyboard_protocol_changes, 2);

        core.advance(b"\x1b[?1049h\x1b[=8u").unwrap();
        assert_eq!(core.input_modes().keyboard.kitty.bits(), 8);
        core.advance(b"\x1b[?1049l").unwrap();
        assert_eq!(core.input_modes().keyboard.kitty.bits(), 3);
        core.advance(b"\x1bc").unwrap();
        assert_eq!(
            core.input_modes().keyboard,
            super::super::KeyboardProtocolState::default()
        );
    }

    #[test]
    fn decscusr_and_underline_styles_survive_in_the_snapshot() {
        let config = TerminalCoreConfig {
            history_lines: 0,
            default_cursor_shape: CursorShape::Beam,
        };
        let mut core = TerminalCoreAdapter::new(GridSize::new(12, 2).unwrap(), config).unwrap();

        for (parameter, shape, blink) in [
            (1, CursorShape::Block, CursorBlink::Blinking),
            (2, CursorShape::Block, CursorBlink::Steady),
            (3, CursorShape::Underline, CursorBlink::Blinking),
            (4, CursorShape::Underline, CursorBlink::Steady),
            (5, CursorShape::Beam, CursorBlink::Blinking),
            (6, CursorShape::Beam, CursorBlink::Steady),
        ] {
            core.advance(format!("\x1b[{parameter} q").as_bytes())
                .unwrap();
            let cursor = core.snapshot().unwrap().cursor;
            assert_eq!((cursor.shape, cursor.blink), (shape, blink));
        }
        core.advance(b"\x1b[0 q").unwrap();
        assert_eq!(core.snapshot().unwrap().cursor.shape, CursorShape::Beam);

        core.advance(b"\r\x1b[4m1\x1b[4:2m2\x1b[4:3m3\x1b[4:4m4\x1b[4:5m5\x1b[24m0")
            .unwrap();
        let snapshot = core.snapshot().unwrap();
        assert_eq!(
            snapshot.cells[..6]
                .iter()
                .map(|cell| cell.underline_style)
                .collect::<Vec<_>>(),
            [
                UnderlineStyle::Single,
                UnderlineStyle::Double,
                UnderlineStyle::Curly,
                UnderlineStyle::Dotted,
                UnderlineStyle::Dashed,
                UnderlineStyle::None,
            ]
        );
    }

    #[test]
    fn synchronized_updates_publish_only_when_committed() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 0).unwrap();
        core.advance(b"old").unwrap();
        let before = core.snapshot().unwrap();
        let before_content = core.search_content_id();
        assert!(!core.advance(b"\x1b[?2026hnew").unwrap().dirty);
        assert!(core.pending_sync().is_some());
        assert_eq!(core.search_content_id(), before_content);
        assert_eq!(core.snapshot().unwrap(), before);
        assert!(!core.advance(b"er").unwrap().dirty);
        assert!(core.advance(b"\x1b[?2026l").unwrap().dirty);
        assert!(core.pending_sync().is_none());
        assert_ne!(core.snapshot().unwrap().cells, before.cells);
        assert!(core.search_content_id().content_revision > before_content.content_revision);

        let committed = core.search_content_id();
        core.advance(b"\x1b[?2026hdiscarded").unwrap();
        let discard_epoch = core.pending_sync().unwrap().epoch;
        assert!(core.discard_synchronized_update(discard_epoch));
        assert_eq!(core.search_content_id(), committed);

        core.advance(b"\x1b[?2026hfirst").unwrap();
        let first_epoch = core.pending_sync().unwrap().epoch;
        core.advance(b"\x1b[?2026l\x1b[?2026hsecond").unwrap();
        assert!(core.pending_sync().unwrap().epoch > first_epoch);

        core.advance(b"\x1b[?2026htimeout").unwrap();
        let pending = core.pending_sync().unwrap();
        assert!(
            core.flush_synchronized_update(pending.epoch, SyncFlushReason::Timeout)
                .unwrap()
                .dirty
        );
        assert!(
            core.flush_synchronized_update(pending.epoch, SyncFlushReason::Timeout)
                .unwrap()
                .audit
                .sync_timeouts
                == 0
        );
        assert!(pending.epoch >= 1);
    }

    #[test]
    fn approved_queries_are_typed_and_color_mutations_remain_blocked() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(20, 4).unwrap(), 0).unwrap();
        let delta = core
            .advance(b"\x1b]10;?\x1b\\\x1b]11;?\x07\x1b[14t\x1b[18t\x1b]10;#ffffff\x07\x1b]112\x07")
            .unwrap();
        let mut actions = Vec::new();
        core.drain_actions(&mut actions);
        assert_eq!(
            actions,
            [
                TerminalAction::Query(TerminalQuery::DefaultColor {
                    slot: DefaultColorSlot::Foreground,
                    terminator: QueryTerminator::StringTerminator,
                }),
                TerminalAction::Query(TerminalQuery::DefaultColor {
                    slot: DefaultColorSlot::Background,
                    terminator: QueryTerminator::Bell,
                }),
                TerminalAction::Query(TerminalQuery::TextAreaPixels),
                TerminalAction::WriteToPty(b"\x1b[8;4;20t".to_vec()),
            ]
        );
        assert_eq!(delta.audit.rejected_actions, 2);
    }

    #[test]
    fn display_protocol_state_is_isolated_between_terminal_cores() {
        let mut first = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 0).unwrap();
        let mut second = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 0).unwrap();
        first
            .advance(b"\x1b[5 q\x1b[4:3mA\x1b[?2026hpending")
            .unwrap();
        second.advance(b"B").unwrap();

        let first_snapshot = first.snapshot().unwrap();
        let second_snapshot = second.snapshot().unwrap();
        assert_eq!(first_snapshot.cursor.shape, CursorShape::Beam);
        assert_eq!(
            first_snapshot.cells[0].underline_style,
            UnderlineStyle::Curly
        );
        assert!(first.pending_sync().is_some());
        assert_eq!(second_snapshot.cursor.shape, CursorShape::Block);
        assert_eq!(
            second_snapshot.cells[0].underline_style,
            UnderlineStyle::None
        );
        assert!(second.pending_sync().is_none());
    }

    #[test]
    fn osc52_set_query_and_clear_are_rejected_without_a_reply() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(20, 4).unwrap(), 100).unwrap();
        let delta = core
            .advance(b"\x1b]52;c;Zm9v\x07\x1b]52;c;?\x07\x1b]52;c;\x07")
            .unwrap();
        let mut actions = Vec::new();
        core.drain_actions(&mut actions);

        assert_eq!(delta.audit.rejected_actions, 3);
        assert_eq!(delta.audit.reply_bytes, 0);
        assert!(
            actions.is_empty(),
            "OSC 52 reached the application boundary"
        );
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
    fn semantic_selection_treats_ascii_and_cjk_punctuation_as_boundaries() {
        for (separator, right_column) in [
            (",", 5),
            ("，", 6),
            ("。", 6),
            (":", 5),
            ("：", 6),
            (";", 5),
            ("；", 6),
        ] {
            let mut core = TerminalCoreAdapter::new(GridSize::new(16, 2).unwrap(), 0).unwrap();
            core.advance(format!("left{separator}right").as_bytes())
                .unwrap();

            for (column, expected) in [(1, "left"), (right_column, "right")] {
                let point = SelectionPoint { column, line: 0 };
                core.start_selection(SelectionKind::Semantic, point, SelectionSide::Left)
                    .unwrap();
                core.update_selection(point, SelectionSide::Right).unwrap();
                assert_eq!(
                    core.selected_text().as_deref(),
                    Some(expected),
                    "separator {separator:?} did not terminate semantic selection"
                );
            }
        }

        let mut core = TerminalCoreAdapter::new(GridSize::new(16, 2).unwrap(), 0).unwrap();
        core.advance(b"left.right").unwrap();
        let point = SelectionPoint { column: 1, line: 0 };
        core.start_selection(SelectionKind::Semantic, point, SelectionSide::Left)
            .unwrap();
        core.update_selection(point, SelectionSide::Right).unwrap();
        assert_eq!(core.selected_text().as_deref(), Some("left.right"));
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
    fn absolute_scrollback_offset_is_clamped_to_the_frozen_history_size() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 10).unwrap();
        core.advance(b"one\r\ntwo\r\nthree\r\nfour").unwrap();
        let history = core.snapshot().unwrap().history_size;
        assert!(history > 0);
        core.scroll_to_display_offset(usize::MAX).unwrap();
        assert_eq!(core.snapshot().unwrap().display_offset, history);
        core.scroll_to_display_offset(0).unwrap();
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

    fn search_all(core: &TerminalCoreAdapter, query: &str) -> Vec<SearchMatch> {
        let compiled = core.compile_literal_search(query).unwrap();
        let mut cursor = Some(core.search_scan_cursor());
        let mut matches = Vec::new();
        while let Some(current) = cursor {
            let step = core
                .scan_search_slice(
                    &compiled,
                    current,
                    SearchBudget {
                        deadline: Instant::now() + std::time::Duration::from_secs(1),
                        max_rows: 256,
                        max_matches: 10_000,
                        clock: Instant::now,
                    },
                )
                .unwrap();
            matches.extend(step.matches);
            cursor = step.next;
        }
        matches
    }

    fn regex_search_all(core: &TerminalCoreAdapter, query: &str) -> Vec<SearchMatch> {
        let compiled = core.compile_regex_search(query).unwrap();
        let mut cursor = Some(core.regex_scan_cursor());
        let mut matches = Vec::new();
        while let Some(current) = cursor {
            let step = core
                .scan_regex_slice(
                    &compiled,
                    current,
                    SearchBudget {
                        deadline: Instant::now() + std::time::Duration::from_secs(1),
                        max_rows: 256,
                        max_matches: 10_000,
                        clock: Instant::now,
                    },
                )
                .unwrap();
            matches.extend(step.matches);
            cursor = step.next;
        }
        matches
    }

    #[test]
    fn search_content_revision_excludes_viewport_scrolling() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 10).unwrap();
        let initial = core.search_content_id();
        core.advance(b"one\r\ntwo\r\nthree").unwrap();
        let changed = core.search_content_id();
        assert!(changed.content_revision > initial.content_revision);
        core.scroll_display(1).unwrap();
        assert_eq!(core.search_content_id(), changed);
        assert!(core.snapshot().unwrap().generation > initial.content_revision);
    }

    #[test]
    fn literal_search_handles_overlap_metacharacters_and_case() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(16, 2).unwrap(), 10).unwrap();
        core.advance(b"aaa .* Aa").unwrap();
        assert_eq!(search_all(&core, "aa").len(), 2);
        assert_eq!(search_all(&core, ".*").len(), 1);
        assert_eq!(search_all(&core, "Aa").len(), 1);
        assert!(search_all(&core, "aA").is_empty());
    }

    #[test]
    fn literal_search_crosses_soft_wrap_but_not_hard_break() {
        let mut wrapped = TerminalCoreAdapter::new(GridSize::new(4, 2).unwrap(), 10).unwrap();
        wrapped.advance(b"abcdef").unwrap();
        let matches = search_all(&wrapped, "cdef");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start.column, 2);
        assert_eq!(matches[0].end.column, 1);

        let mut hard = TerminalCoreAdapter::new(GridSize::new(4, 2).unwrap(), 10).unwrap();
        hard.advance(b"ab\r\ncd").unwrap();
        assert!(search_all(&hard, "abcd").is_empty());
    }

    #[test]
    fn literal_search_tracks_combining_scalars_and_skips_wide_spacers() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 10).unwrap();
        core.advance("e\u{301}中x".as_bytes()).unwrap();
        let combining = search_all(&core, "\u{301}");
        assert_eq!(combining[0].start.scalar_offset, 1);
        let wide = search_all(&core, "中x");
        assert_eq!(wide.len(), 1);
        assert_eq!(wide[0].start.column, 1);
        assert_eq!(wide[0].end.column, 3);
    }

    #[test]
    fn literal_search_enforces_query_and_slice_limits() {
        let core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 10).unwrap();
        assert_eq!(
            core.compile_literal_search("\n").unwrap_err(),
            SearchError::ControlCharacter
        );
        assert_eq!(
            core.compile_literal_search(&"x".repeat(super::super::MAX_SEARCH_QUERY_SCALARS + 1))
                .unwrap_err(),
            SearchError::QueryTooLong
        );
        let compiled = core.compile_literal_search(" ").unwrap();
        let step = core
            .scan_search_slice(
                &compiled,
                core.search_scan_cursor(),
                SearchBudget {
                    deadline: Instant::now() + std::time::Duration::from_secs(1),
                    max_rows: 1,
                    max_matches: 2,
                    clock: Instant::now,
                },
            )
            .unwrap();
        assert!(step.scanned_rows <= 1);
        assert_eq!(step.matches.len(), 2);
        assert!(step.next.is_some());
    }

    #[test]
    fn regex_search_is_bounded_and_preserves_terminal_line_semantics() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 3).unwrap(), 10).unwrap();
        core.advance(b"error erring abcdef\r\nghi").unwrap();
        assert_eq!(regex_search_all(&core, r"err(or|ing)").len(), 2);
        assert_eq!(regex_search_all(&core, r"cdef").len(), 1);
        assert!(regex_search_all(&core, r"fg.*hi").is_empty());
        assert!(matches!(
            core.compile_regex_search("["),
            Err(SearchError::InvalidRegex)
        ));
        assert!(core.compile_regex_search(&"(".repeat(256)).is_err());
    }

    #[test]
    fn bell_flood_is_coalesced_before_the_event_and_action_vectors() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(4, 2).unwrap(), 10).unwrap();
        let bells = vec![b'\x07'; ByteBatch::MAX_LEN];
        let delta = core.advance(&bells).unwrap();
        assert_eq!(delta.actions, 1);
        assert!(core.events.borrow().events.is_empty());
        let mut actions = Vec::new();
        core.drain_actions(&mut actions);
        assert_eq!(actions, [TerminalAction::Bell]);
    }
}

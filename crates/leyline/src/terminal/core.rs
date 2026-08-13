use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    term::{Config, TermMode, cell::Flags, test::TermSize},
    vte::ansi::{self, Color},
};

use super::snapshot::{
    CellFlags, CellWidth, CursorSnapshot, FrameSnapshot, GridSize, SnapshotCell, SnapshotHyperlink,
    TerminalColor, TerminalModes,
};

const MAX_TITLE_BYTES: usize = 1024;
const MAX_REPLY_BYTES: usize = 64 * 1024;
const MAX_ZERO_WIDTH_PER_CELL: usize = 64;
const MAX_ZERO_WIDTH_TOTAL: usize = 64 * 1024;
const MAX_HYPERLINKS: usize = 4096;
const MAX_HYPERLINK_BYTES: usize = 4096;

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
            _main_thread: Rc::new(()),
        })
    }

    pub fn advance(&mut self, bytes: &[u8]) -> Result<TerminalDelta, TerminalError> {
        if bytes.is_empty() {
            return Ok(TerminalDelta::default());
        }
        self.parser.advance(&mut self.term, bytes);
        self.audit_grid()?;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TerminalError::GenerationOverflow)?;
        self.cached.get_mut().take();
        self.collect_events();
        Ok(TerminalDelta {
            dirty: true,
            actions: self.actions.len(),
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
        for indexed in content.display_iter {
            let cell = indexed.cell;
            let hyperlink = if let Some(link) = cell.hyperlink() {
                if link.id().len() > MAX_HYPERLINK_BYTES || link.uri().len() > MAX_HYPERLINK_BYTES {
                    return Err(TerminalError::HyperlinkTooLarge);
                }
                let key = (link.id().to_owned(), link.uri().to_owned());
                if let Some(id) = link_ids.get(&key) {
                    Some(*id)
                } else {
                    if links.len() >= MAX_HYPERLINKS {
                        return Err(TerminalError::TooManyHyperlinks);
                    }
                    let id =
                        u16::try_from(links.len()).map_err(|_| TerminalError::TooManyHyperlinks)?;
                    links.push(SnapshotHyperlink {
                        id: Arc::from(key.0.as_str()),
                        uri: Arc::from(key.1.as_str()),
                    });
                    link_ids.insert(key, id);
                    Some(id)
                }
            } else {
                None
            };
            cells.push(SnapshotCell {
                ch: cell.c,
                zerowidth: cell.zerowidth().filter(|v| !v.is_empty()).map(Arc::from),
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
                } else if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
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
                visible: mode.contains(TermMode::SHOW_CURSOR),
            },
            modes: TerminalModes {
                alternate_screen: mode.contains(TermMode::ALT_SCREEN),
                bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
                application_cursor: mode.contains(TermMode::APP_CURSOR),
                mouse_reporting: mode.intersects(TermMode::MOUSE_MODE),
            },
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

    fn audit_grid(&self) -> Result<(), TerminalError> {
        let mut total = 0;
        for indexed in self.term.grid().display_iter() {
            let count = indexed.cell.zerowidth().map_or(0, <[char]>::len);
            if count > MAX_ZERO_WIDTH_PER_CELL {
                return Err(TerminalError::CombiningLimit);
            }
            total += count;
        }
        if total > MAX_ZERO_WIDTH_TOTAL {
            return Err(TerminalError::CombiningLimit);
        }
        Ok(())
    }

    fn collect_events(&mut self) {
        let events: Vec<_> = self.events.borrow_mut().drain(..).collect();
        for event in events {
            match event {
                Event::Title(title) => {
                    let title = bounded_title(&title);
                    self.title = Some(title.clone());
                    self.actions.push(TerminalAction::SetTitle(title));
                }
                Event::ResetTitle => {
                    self.title = None;
                }
                Event::Bell => self.actions.push(TerminalAction::Bell),
                Event::PtyWrite(text) if text.len() <= MAX_REPLY_BYTES => self
                    .actions
                    .push(TerminalAction::WriteToPty(text.into_bytes())),
                Event::ClipboardStore(..) | Event::ClipboardLoad(..) => {
                    self.actions.push(TerminalAction::ClipboardRequestRejected);
                }
                Event::ColorRequest(..) | Event::TextAreaSizeRequest(..) => {
                    self.actions.push(TerminalAction::UnsupportedSequence);
                }
                _ => {}
            }
        }
    }
}

fn bounded_title(value: &str) -> Arc<str> {
    Arc::from(
        value
            .chars()
            .filter(|ch| !ch.is_control())
            .collect::<String>()
            .chars()
            .take(MAX_TITLE_BYTES)
            .collect::<String>(),
    )
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
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_utf8_color_modes_title_and_rejects_clipboard() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(20, 4).unwrap(), 100).unwrap();
        core.advance(b"A\xe4\xb8\xad\x1b[38;2;1;2;3mR\x1b[?2004h\x1b]0;safe\x07\x1b]52;c;Zm9v\x07")
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
    }
    #[test]
    fn snapshot_reuses_cell_storage_for_unchanged_generation() {
        let core = TerminalCoreAdapter::new(GridSize::new(2, 2).unwrap(), 0).unwrap();
        let first = core.snapshot().unwrap();
        let second = core.snapshot().unwrap();
        assert!(Arc::ptr_eq(&first.cells, &second.cells));
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
        assert!(snapshot.modes.application_cursor && snapshot.modes.mouse_reporting);
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
}

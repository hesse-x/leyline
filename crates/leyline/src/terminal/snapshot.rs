use std::{num::NonZeroU16, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GridSize {
    pub columns: NonZeroU16,
    pub lines: NonZeroU16,
}

impl GridSize {
    pub const MAX_COLUMNS: u16 = 512;
    pub const MAX_LINES: u16 = 256;
    /// Creates bounded, nonzero terminal dimensions.
    ///
    /// # Errors
    /// Returns a typed error for zero or excessive dimensions.
    pub fn new(columns: u16, lines: u16) -> Result<Self, GridSizeError> {
        if columns > Self::MAX_COLUMNS || lines > Self::MAX_LINES {
            return Err(GridSizeError::TooLarge);
        }
        Ok(Self {
            columns: NonZeroU16::new(columns).ok_or(GridSizeError::Zero)?,
            lines: NonZeroU16::new(lines).ok_or(GridSizeError::Zero)?,
        })
    }
    #[must_use]
    pub const fn columns(self) -> usize {
        self.columns.get() as usize
    }
    #[must_use]
    pub const fn lines(self) -> usize {
        self.lines.get() as usize
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GridSizeError {
    #[error("terminal grid dimensions must be nonzero")]
    Zero,
    #[error("terminal grid exceeds 512x256 cells")]
    TooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameSnapshot {
    pub generation: u64,
    pub content_revision: u64,
    pub active_buffer: SearchBuffer,
    pub grid: GridSize,
    pub cells: Arc<[SnapshotCell]>,
    pub cursor: CursorSnapshot,
    pub modes: TerminalModes,
    pub display_offset: usize,
    pub history_size: usize,
    pub title: Option<Arc<str>>,
    pub hyperlinks: Arc<[SnapshotHyperlink]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchBuffer {
    Normal,
    Alternate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotCell {
    pub ch: char,
    pub zerowidth: Option<Arc<[char]>>,
    pub foreground: TerminalColor,
    pub background: TerminalColor,
    pub underline_color: Option<TerminalColor>,
    pub underline_style: UnderlineStyle,
    pub flags: CellFlags,
    pub width: CellWidth,
    pub hyperlink: Option<u16>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalColor {
    Named(u16),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct CellFlags {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikeout: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CellWidth {
    Narrow,
    Wide,
    Spacer,
    LeadingSpacer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorSnapshot {
    pub column: u16,
    pub line: u16,
    pub visible: bool,
    pub shape: CursorShape,
    pub blink: CursorBlink,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CursorShape {
    #[default]
    Block,
    Beam,
    Underline,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CursorBlink {
    #[default]
    Steady,
    Blinking,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TerminalModes {
    pub alternate_screen: bool,
    pub bracketed_paste: bool,
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub keyboard: KeyboardProtocolState,
    pub focus_reporting: bool,
    pub alternate_scroll: bool,
    pub mouse_protocol: MouseProtocol,
    pub mouse_encoding: MouseEncoding,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyboardProtocolState {
    pub kitty: KittyKeyboardFlags,
    pub modify_other_keys: ModifyOtherKeysLevel,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KittyKeyboardFlags(u8);

impl KittyKeyboardFlags {
    pub const DISAMBIGUATE: Self = Self(1);
    pub const REPORT_EVENTS: Self = Self(2);
    pub const ALTERNATE_KEYS: Self = Self(4);
    pub const ALL_KEYS: Self = Self(8);
    pub const ASSOCIATED_TEXT: Self = Self(16);
    pub const VALID_BITS: u8 = 31;

    #[must_use]
    pub const fn from_valid_bits(bits: u8) -> Option<Self> {
        if bits & !Self::VALID_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[must_use]
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum ModifyOtherKeysLevel {
    #[default]
    Disabled = 0,
    ExceptWellDefined = 1,
    All = 2,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MouseProtocol {
    #[default]
    None,
    X10,
    Normal,
    ButtonEvent,
    AnyEvent,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MouseEncoding {
    #[default]
    Legacy,
    Utf8,
    Sgr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotHyperlink {
    pub id: Arc<str>,
    pub uri: Arc<str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionKind {
    Simple,
    Semantic,
    Lines,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionSide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionPoint {
    pub column: u16,
    pub line: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectedSelection {
    pub start: [u16; 2],
    pub end: [u16; 2],
}

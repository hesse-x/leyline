mod core;
pub mod cwd;
mod debug_grid;
mod input;
mod protocol;
mod search;
mod snapshot;

pub use core::{
    DefaultColorSlot, ParseAuditDelta, PendingSync, QueryTerminator, SyncFlushReason,
    TerminalAction, TerminalCoreAdapter, TerminalCoreConfig, TerminalDelta, TerminalError,
    TerminalQuery,
};
pub use debug_grid::format_debug_grid;
pub use input::{
    ButtonState, EncodedKey, IgnoreReason, InputError, KeyboardEventKind, MAX_PASTE_BODY_BYTES,
    MAX_TRANSACTION_BYTES, Modifiers, MouseButton, TerminalKey, TerminalKeyboardEvent, commit_text,
    encode_alternate_scroll, encode_focus, encode_key, encode_keyboard_event, encode_mouse,
    paste_transaction,
};
pub use search::{
    CompiledLiteral, CompiledRegex, MAX_REGEX_LOGICAL_LINE_BYTES, MAX_SEARCH_MATCHES,
    MAX_SEARCH_QUERY_BYTES, MAX_SEARCH_QUERY_SCALARS, MAX_SEARCH_SLICE_ROWS, RegexScanCursor,
    RegexScanStep, SearchAnchor, SearchBudget, SearchContentId, SearchError, SearchMatch,
    SearchProjection, SearchScanCursor, SearchScanStep,
};
pub use snapshot::{
    CellFlags, CellWidth, CursorBlink, CursorShape, CursorSnapshot, FrameSnapshot, GridSize,
    KeyboardProtocolState, KittyKeyboardFlags, ModifyOtherKeysLevel, MouseEncoding, MouseProtocol,
    ProjectedSelection, SearchBuffer, SelectionKind, SelectionPoint, SelectionSide, SnapshotCell,
    SnapshotHyperlink, TerminalColor, TerminalModes, UnderlineStyle,
};

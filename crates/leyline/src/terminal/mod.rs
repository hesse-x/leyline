mod core;
mod debug_grid;
mod input;
mod snapshot;

pub use core::{
    ParseAuditDelta, TerminalAction, TerminalCoreAdapter, TerminalDelta, TerminalError,
};
pub use debug_grid::format_debug_grid;
pub use input::{
    ButtonState, InputError, MAX_PASTE_BODY_BYTES, MAX_TRANSACTION_BYTES, Modifiers, MouseButton,
    TerminalKey, commit_text, encode_alternate_scroll, encode_focus, encode_key, encode_mouse,
    paste_transaction,
};
pub use snapshot::{
    CellFlags, CellWidth, CursorSnapshot, FrameSnapshot, GridSize, MouseEncoding, MouseProtocol,
    ProjectedSelection, SelectionKind, SelectionPoint, SelectionSide, SnapshotCell,
    SnapshotHyperlink, TerminalColor, TerminalModes,
};

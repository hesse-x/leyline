use std::fmt::Write;

use super::{CellWidth, FrameSnapshot};

/// Produces a bounded, escaped diagnostic view of the visible terminal grid.
#[must_use]
pub fn format_debug_grid(snapshot: &FrameSnapshot) -> String {
    let mut output = String::with_capacity(snapshot.cells.len().saturating_add(256));
    let _ = writeln!(
        output,
        "generation={} grid={}x{} cursor={},{} visible={} offset={} alt={} paste={} mouse={}",
        snapshot.generation,
        snapshot.grid.columns,
        snapshot.grid.lines,
        snapshot.cursor.column,
        snapshot.cursor.line,
        snapshot.cursor.visible,
        snapshot.display_offset,
        snapshot.modes.alternate_screen,
        snapshot.modes.bracketed_paste,
        snapshot.modes.mouse_reporting
    );
    for row in snapshot.cells.chunks(snapshot.grid.columns()) {
        for cell in row {
            let ch = if cell.flags.hidden || matches!(cell.width, CellWidth::Spacer) {
                ' '
            } else if cell.ch.is_control() {
                '\u{fffd}'
            } else {
                cell.ch
            };
            output.push(ch);
            if let Some(combining) = &cell.zerowidth {
                output.extend(combining.iter().copied().filter(|ch| !ch.is_control()));
            }
        }
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalCoreAdapter;

    #[test]
    fn debug_grid_escapes_controls_and_reports_state() {
        let mut core =
            TerminalCoreAdapter::new(super::super::GridSize::new(4, 2).unwrap(), 0).unwrap();
        core.advance(b"A\x01B").unwrap();
        let text = format_debug_grid(&core.snapshot().unwrap());
        assert!(text.contains("grid=4x2"));
        assert!(!text.contains('\x01'));
    }
}

use std::fmt::Write;

use super::{CellWidth, FrameSnapshot, SnapshotCell, TerminalColor};

const MAX_DIAGNOSTIC_CELLS: usize = 4096;

/// Produces a bounded, escaped diagnostic view of the visible terminal grid.
#[must_use]
pub fn format_debug_grid(snapshot: &FrameSnapshot) -> String {
    let mut output = String::with_capacity(snapshot.cells.len().saturating_add(256));
    let _ = writeln!(
        output,
        "generation={} grid={}x{} cursor={},{} visible={} offset={}/{} alt={} paste={} focus={} mouse={:?}/{:?}",
        snapshot.generation,
        snapshot.grid.columns,
        snapshot.grid.lines,
        snapshot.cursor.column,
        snapshot.cursor.line,
        snapshot.cursor.visible,
        snapshot.display_offset,
        snapshot.history_size,
        snapshot.modes.alternate_screen,
        snapshot.modes.bracketed_paste,
        snapshot.modes.focus_reporting,
        snapshot.modes.mouse_protocol,
        snapshot.modes.mouse_encoding
    );
    for row in snapshot.cells.chunks(snapshot.grid.columns()) {
        for cell in row {
            let ch = if cell.flags.hidden
                || matches!(cell.width, CellWidth::Spacer | CellWidth::LeadingSpacer)
            {
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
    for (index, cell) in snapshot.cells.iter().take(MAX_DIAGNOSTIC_CELLS).enumerate() {
        if is_default_cell(cell) {
            continue;
        }
        let column = index % snapshot.grid.columns();
        let line = index / snapshot.grid.columns();
        let _ = write!(
            output,
            "cell={column},{line} ch=U+{:04X} width={:?} fg=",
            u32::from(cell.ch),
            cell.width
        );
        write_color(&mut output, cell.foreground);
        output.push_str(" bg=");
        write_color(&mut output, cell.background);
        output.push_str(" ul=");
        if let Some(color) = cell.underline_color {
            write_color(&mut output, color);
        } else {
            output.push_str("none");
        }
        let flags = cell.flags;
        let _ = writeln!(
            output,
            " flags={}{}{}{}{}{}{} combining={} hyperlink={}",
            flag(flags.bold, 'b'),
            flag(flags.dim, 'd'),
            flag(flags.italic, 'i'),
            flag(flags.underline, 'u'),
            flag(flags.inverse, 'v'),
            flag(flags.hidden, 'h'),
            flag(flags.strikeout, 's'),
            cell.zerowidth.as_ref().map_or(0, |value| value.len()),
            cell.hyperlink
                .map_or_else(|| "none".to_owned(), |id| id.to_string())
        );
    }
    if snapshot.cells.len() > MAX_DIAGNOSTIC_CELLS {
        let _ = writeln!(
            output,
            "cells_truncated={}",
            snapshot.cells.len() - MAX_DIAGNOSTIC_CELLS
        );
    }
    output
}

fn is_default_cell(cell: &SnapshotCell) -> bool {
    cell.ch == ' '
        && cell.zerowidth.is_none()
        && cell.foreground == TerminalColor::Named(256)
        && cell.background == TerminalColor::Named(257)
        && cell.underline_color.is_none()
        && cell.flags == super::CellFlags::default()
        && cell.width == CellWidth::Narrow
        && cell.hyperlink.is_none()
}

fn write_color(output: &mut String, color: TerminalColor) {
    match color {
        TerminalColor::Named(index) => {
            let _ = write!(output, "named:{index}");
        }
        TerminalColor::Indexed(index) => {
            let _ = write!(output, "indexed:{index}");
        }
        TerminalColor::Rgb(red, green, blue) => {
            let _ = write!(output, "rgb:{red:02x}{green:02x}{blue:02x}");
        }
    }
}

const fn flag(enabled: bool, name: char) -> char {
    if enabled { name } else { '-' }
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

    #[test]
    fn debug_grid_is_a_structured_tmux_scene_oracle() {
        let mut core =
            TerminalCoreAdapter::new(super::super::GridSize::new(12, 3).unwrap(), 0).unwrap();
        core.advance(
            b"\x1b[1;3;4;9;38;5;196;48;5;17mA\x1b[0me\xcc\x81\xe4\xb8\xad\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[?2004h",
        )
        .unwrap();

        let text = format_debug_grid(&core.snapshot().unwrap());
        assert!(text.contains("paste=true focus=true mouse=Normal/Sgr"));
        assert!(text.contains(
            "cell=0,0 ch=U+0041 width=Narrow fg=indexed:196 bg=indexed:17 ul=none flags=b-iu--s"
        ));
        assert!(text.contains("cell=1,0 ch=U+0065 width=Narrow"));
        assert!(text.contains("combining=1"));
        assert!(text.contains("cell=2,0 ch=U+4E2D width=Wide"));
        assert!(text.contains("cell=3,0 ch=U+0020 width=Spacer"));
    }
}

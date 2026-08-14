use std::{fs, path::Path};

use leyline::terminal::{
    ButtonState, CellWidth, GridSize, Modifiers, MouseButton, MouseEncoding, MouseProtocol,
    TerminalColor, TerminalCoreAdapter, encode_focus, encode_mouse, format_debug_grid,
    paste_transaction,
};

use crate::report::{ProbeError, ProbeResult, Reporter};

const MAX_FIXTURE_BYTES: usize = 1024 * 1024;
const BUILTIN_FIXTURE: &[u8] = b"\x1b[2J\x1b[Hpane-1 | pane-2\r\n\x1b[1;38;5;196;48;5;17mstatus\x1b[0m e\xcc\x81 \xe4\xb8\xad\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b]0;tmux scene\x07\x1b]52;c;Zm9v\x07";

pub fn run(reporter: &mut Reporter, fixture: Option<&Path>) -> ProbeResult<()> {
    let bytes = read_fixture(fixture)?;
    let mut core = TerminalCoreAdapter::new(
        GridSize::new(80, 24).map_err(|error| {
            ProbeError::internal("scene.grid", format!("invalid probe grid: {error}"))
        })?,
        2_000,
    )
    .map_err(|error| ProbeError::internal("scene.core", error.to_string()))?;
    let delta = core
        .advance(&bytes)
        .map_err(|error| ProbeError::internal("scene.parse", error.to_string()))?;
    let snapshot = core
        .snapshot()
        .map_err(|error| ProbeError::internal("scene.snapshot", error.to_string()))?;

    validate_cells(&snapshot)?;
    validate_interactions(snapshot.modes)?;
    if snapshot.title.as_deref() != Some("tmux scene") {
        return Err(ProbeError::internal(
            "scene.title",
            "title update did not reach the snapshot boundary",
        ));
    }
    if delta.audit.rejected_actions == 0 {
        return Err(ProbeError::internal(
            "scene.security",
            "OSC 52 was not rejected while parsing the scene",
        ));
    }

    reporter.pass(
        "scene",
        "snapshot",
        format!(
            "{} bytes; pane/status text, 256-color attributes, Unicode widths, and P0 interactions preserved",
            bytes.len()
        ),
    );
    reporter.pass(
        "scene",
        "security-boundary",
        "OSC 52 rejected without including its payload in the diagnostic",
    );
    Ok(())
}

fn read_fixture(fixture: Option<&Path>) -> ProbeResult<Vec<u8>> {
    let bytes = match fixture {
        Some(path) => fs::read(path).map_err(|error| {
            ProbeError::missing(
                "scene.fixture",
                format!("{}: {error}", path.display()),
                "provide a readable tmux output fixture",
            )
        })?,
        None => BUILTIN_FIXTURE.to_vec(),
    };
    if bytes.len() > MAX_FIXTURE_BYTES {
        return Err(ProbeError::unsuitable(
            "scene.fixture",
            format!(
                "fixture is {} bytes; limit is {MAX_FIXTURE_BYTES}",
                bytes.len()
            ),
            "reduce the fixture size",
        ));
    }

    Ok(bytes)
}

fn validate_cells(snapshot: &leyline::terminal::FrameSnapshot) -> ProbeResult<()> {
    let diagnostic = format_debug_grid(snapshot);

    for expected in ["pane-1", "pane-2", "status", "中"] {
        if !diagnostic.contains(expected) {
            return Err(ProbeError::internal(
                "scene.cells",
                format!("visible grid does not contain {expected:?}"),
            ));
        }
    }
    if !snapshot.cells.iter().any(|cell| {
        cell.ch == 's'
            && cell.foreground == TerminalColor::Indexed(196)
            && cell.background == TerminalColor::Indexed(17)
            && cell.flags.bold
    }) {
        return Err(ProbeError::internal(
            "scene.attributes",
            "indexed color and bold status attributes were not preserved",
        ));
    }
    if !snapshot
        .cells
        .iter()
        .any(|cell| cell.ch == 'e' && cell.zerowidth.as_deref() == Some(['\u{301}'].as_slice()))
        || !snapshot
            .cells
            .iter()
            .any(|cell| cell.ch == '中' && cell.width == CellWidth::Wide)
    {
        return Err(ProbeError::internal(
            "scene.width",
            "combining or wide-cell structure was not preserved",
        ));
    }
    Ok(())
}

fn validate_interactions(modes: leyline::terminal::TerminalModes) -> ProbeResult<()> {
    if modes.mouse_protocol != MouseProtocol::Normal
        || modes.mouse_encoding != MouseEncoding::Sgr
        || !modes.focus_reporting
        || !modes.bracketed_paste
    {
        return Err(ProbeError::internal(
            "scene.modes",
            format!("P0 interaction modes are incomplete: {modes:?}"),
        ));
    }
    let focus = encode_focus(true, modes);
    let mouse = encode_mouse(
        MouseButton::Left,
        ButtonState::Pressed,
        2,
        3,
        Modifiers::default(),
        modes,
    )
    .map_err(|error| ProbeError::internal("scene.mouse", error.to_string()))?;
    let paste = paste_transaction("one\ntwo", modes.bracketed_paste)
        .map_err(|error| ProbeError::internal("scene.paste", error.to_string()))?;
    if focus.as_deref() != Some(b"\x1b[I")
        || mouse.as_deref() != Some(b"\x1b[<0;3;4M")
        || paste != b"\x1b[200~one\rtwo\x1b[201~"
    {
        return Err(ProbeError::internal(
            "scene.interaction",
            "focus, SGR mouse, or Bracketed Paste encoding disagrees with the parsed modes",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_fixture_crosses_the_product_snapshot_boundary() {
        run(&mut Reporter::new(false, false), None).unwrap();
    }
}

use std::{fs, path::Path};

use leyline::terminal::{
    ButtonState, CellWidth, CursorBlink, CursorShape, GridSize, Modifiers, MouseButton,
    MouseEncoding, MouseProtocol, TerminalColor, TerminalCoreAdapter, UnderlineStyle, encode_focus,
    encode_mouse, format_debug_grid, paste_transaction,
};

use crate::report::{ProbeError, ProbeResult, Reporter};

const MAX_FIXTURE_BYTES: usize = 1024 * 1024;
const BUILTIN_FIXTURE: &[u8] = b"\x1b[2J\x1b[Hpane-1 | pane-2\r\n\x1b[1;38;5;196;48;5;17mstatus\x1b[0m e\xcc\x81 \xe4\xb8\xad\r\n\x1b[4m1\x1b[4:2m2\x1b[4:3m3\x1b[4:4m4\x1b[4:5m5\x1b[24m0\x1b[5 q\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b]0;tmux scene\x07\x1b]52;c;Zm9v\x07";

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
    validate_display_protocol(&snapshot)?;
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
            "{} bytes; pane/status text, cursor, underline, colors, Unicode widths, and P0 interactions preserved",
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

fn validate_display_protocol(snapshot: &leyline::terminal::FrameSnapshot) -> ProbeResult<()> {
    if snapshot.cursor.shape != CursorShape::Beam || snapshot.cursor.blink != CursorBlink::Blinking
    {
        return Err(ProbeError::internal(
            "scene.cursor",
            format!(
                "DECSCUSR cursor state was not preserved: {:?}",
                snapshot.cursor
            ),
        ));
    }
    let styles = snapshot
        .cells
        .iter()
        .filter_map(|cell| match cell.ch {
            '1' | '2' | '3' | '4' | '5' | '0' => Some((cell.ch, cell.underline_style)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = [
        ('1', UnderlineStyle::Single),
        ('2', UnderlineStyle::Double),
        ('3', UnderlineStyle::Curly),
        ('4', UnderlineStyle::Dotted),
        ('5', UnderlineStyle::Dashed),
        ('0', UnderlineStyle::None),
    ];
    if !expected.iter().all(|expected| styles.contains(expected)) {
        return Err(ProbeError::internal(
            "scene.underline",
            format!("underline styles differ: {styles:?}"),
        ));
    }
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
    if !matches!(
        modes.mouse_protocol,
        MouseProtocol::Normal | MouseProtocol::ButtonEvent
    ) || modes.mouse_encoding != MouseEncoding::Sgr
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

    #[test]
    fn tmux_drag_mouse_mode_satisfies_the_interaction_contract() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(10, 2).unwrap(), 0).unwrap();
        core.advance(b"\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h")
            .unwrap();
        let modes = core.snapshot().unwrap().modes;
        validate_interactions(modes).unwrap();
    }
}

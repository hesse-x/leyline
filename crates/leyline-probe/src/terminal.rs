use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi;

use crate::report::{ProbeError, ProbeResult, Reporter};

const MAX_FIXTURE_BYTES: usize = 1024 * 1024;
const BUILTIN_FIXTURE: &[u8] = b"ASCII \xE4\xB8\xAD e\xCC\x81\r\n\x1b[38;2;1;2;3mRGB\x1b[0m\x1b[?1049hALT\x1b[?1049l\x1b[?2004h\x1b[?1000h\x1b]0;Leyline Probe\x07\x1b]52;c;Zm9v\x07";

#[derive(Clone, Default)]
struct Listener(Rc<RefCell<Vec<&'static str>>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let name = match event {
            Event::Title(_) => "title",
            Event::ClipboardStore(_, _) | Event::ClipboardLoad(_, _) => "clipboard",
            Event::PtyWrite(_) => "pty-write",
            Event::Bell => "bell",
            _ => "other",
        };
        self.0.borrow_mut().push(name);
    }
}

pub fn run(reporter: &mut Reporter, fixture: Option<&Path>) -> ProbeResult<()> {
    let bytes = match fixture {
        Some(path) => fs::read(path).map_err(|error| {
            ProbeError::missing(
                "terminal.fixture",
                format!("{}: {error}", path.display()),
                "provide a readable byte fixture",
            )
        })?,
        None => BUILTIN_FIXTURE.to_vec(),
    };
    if bytes.len() > MAX_FIXTURE_BYTES {
        return Err(ProbeError::unsuitable(
            "terminal.fixture",
            format!(
                "fixture is {} bytes; limit is {MAX_FIXTURE_BYTES}",
                bytes.len()
            ),
            "reduce the fixture size",
        ));
    }

    let listener = Listener::default();
    let events = Rc::clone(&listener.0);
    let size = TermSize::new(80, 24);
    let mut term = Term::new(Config::default(), &size, listener);
    let mut parser: ansi::Processor = ansi::Processor::new();
    parser.advance(&mut term, &bytes);

    let visible: String = term
        .renderable_content()
        .display_iter
        .map(|indexed| indexed.cell.c)
        .collect();
    for expected in ["ASCII", "中", "RGB"] {
        if !visible.contains(expected) {
            return Err(ProbeError::internal(
                "terminal.grid",
                format!("visible grid does not contain {expected:?}"),
            ));
        }
    }
    let mode = *term.mode();
    for required in [TermMode::BRACKETED_PASTE, TermMode::MOUSE_REPORT_CLICK] {
        if !mode.contains(required) {
            return Err(ProbeError::internal(
                "terminal.mode",
                format!("mode {required:?} was not observable"),
            ));
        }
    }
    term.resize(TermSize::new(100, 30));
    let captured = events.borrow();
    if !captured.contains(&"title") || !captured.contains(&"clipboard") {
        return Err(ProbeError::internal(
            "terminal.events",
            format!("security-sensitive events not intercepted: {captured:?}"),
        ));
    }
    reporter.pass(
        "terminal",
        "embedding",
        format!(
            "{} bytes parsed; grid/modes/resize/events accessible through public API",
            bytes.len()
        ),
    );
    reporter.pass("terminal", "security-boundary", "OSC title and clipboard actions were emitted to the application listener without external side effects");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn builtin_fixture_exercises_public_api() {
        run(&mut Reporter::new(false, false), None).unwrap();
    }

    #[test]
    fn oversized_fixture_is_rejected() {
        let path = std::env::temp_dir().join(format!("leyline-oversized-{}", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("temporary fixture");
        file.write_all(&vec![0; MAX_FIXTURE_BYTES + 1])
            .expect("write fixture");
        drop(file);
        let error = run(&mut Reporter::new(false, false), Some(&path)).expect_err("must reject");
        let _ = std::fs::remove_file(path);
        assert_eq!(error.stage, "terminal.fixture");
        assert_eq!(error.exit_code(), 3);
    }
}

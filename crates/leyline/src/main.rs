#![forbid(unsafe_code)]

use std::{ffi::OsString, io::Write as _, process::ExitCode};

use leyline::{ProcessIo, cli::Verbosity, config::ConfigEnvironment};

struct SystemEnvironment;
impl ConfigEnvironment for SystemEnvironment {
    fn xdg_config_home(&self) -> Option<OsString> {
        std::env::var_os("XDG_CONFIG_HOME")
    }
    fn home(&self) -> Option<OsString> {
        std::env::var_os("HOME")
    }
}

struct Stdio;
impl ProcessIo for Stdio {
    fn stdout(&mut self, text: &str) {
        let _ = std::io::stdout().lock().write_all(text.as_bytes());
    }
    fn stderr(&mut self, text: &str) {
        let _ = std::io::stderr().lock().write_all(text.as_bytes());
    }
    fn install_panic_hook(&mut self) {
        std::panic::set_hook(Box::new(|_| {
            eprintln!(
                "leyline: internal error: unexpected panic; rerun with RUST_BACKTRACE=1 for diagnostics"
            );
        }));
    }
    fn initialize_logging(
        &mut self,
        verbosity: Verbosity,
    ) -> Result<(), leyline::logging::LoggingError> {
        leyline::logging::initialize(verbosity)
    }
    fn graphical_session(&self) -> bool {
        true
    }
}

fn main() -> ExitCode {
    ExitCode::from(leyline::run(std::env::args_os(), SystemEnvironment, &mut Stdio).exit_code())
}

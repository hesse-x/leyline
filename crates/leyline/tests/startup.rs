use std::ffi::OsString;

use leyline::{ProcessIo, cli::Verbosity, config::ConfigEnvironment};

#[derive(Clone)]
struct Env {
    xdg: Option<OsString>,
    home: Option<OsString>,
}
impl ConfigEnvironment for Env {
    fn xdg_config_home(&self) -> Option<OsString> {
        self.xdg.clone()
    }
    fn home(&self) -> Option<OsString> {
        self.home.clone()
    }
}

#[derive(Default)]
struct Io {
    stdout: String,
    stderr: String,
    logging: usize,
    panic_hook: usize,
    fail_logging: bool,
}
impl ProcessIo for Io {
    fn stdout(&mut self, text: &str) {
        self.stdout.push_str(text);
    }
    fn stderr(&mut self, text: &str) {
        self.stderr.push_str(text);
    }
    fn install_panic_hook(&mut self) {
        self.panic_hook += 1;
    }
    fn initialize_logging(&mut self, _: Verbosity) -> Result<(), leyline::logging::LoggingError> {
        self.logging += 1;
        if self.fail_logging {
            Err(leyline::logging::LoggingError::AlreadyInitialized)
        } else {
            Ok(())
        }
    }
}

#[test]
fn help_short_circuits_an_unusable_environment() {
    let mut io = Io::default();
    let result = leyline::run(
        ["leyline", "--help"].map(OsString::from),
        Env {
            xdg: Some("relative".into()),
            home: None,
        },
        &mut io,
    );
    assert_eq!(result.exit_code(), 0);
    assert!(io.stdout.contains("Usage:"));
    assert!(io.stderr.is_empty());
    assert_eq!(io.logging, 0);
    assert_eq!(io.panic_hook, 0);
}

#[test]
fn cli_error_has_stable_exit_code_and_no_startup_side_effects() {
    let mut io = Io::default();
    let result = leyline::run(
        ["leyline", "--unknown"].map(OsString::from),
        Env {
            xdg: None,
            home: None,
        },
        &mut io,
    );
    assert_eq!(result.exit_code(), 2);
    assert!(io.stderr.contains("unexpected argument"));
    assert_eq!(io.logging, 0);
    assert_eq!(io.panic_hook, 0);
}

#[test]
fn logging_setup_failure_is_an_internal_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut io = Io {
        fail_logging: true,
        ..Io::default()
    };
    let result = leyline::run(
        [OsString::from("leyline")],
        Env {
            xdg: Some(temp.path().as_os_str().into()),
            home: None,
        },
        &mut io,
    );
    assert_eq!(result.exit_code(), 4);
    assert!(io.stderr.contains("internal error"));
    assert_eq!(io.panic_hook, 1);
}

#[test]
fn missing_config_uses_defaults_and_exits_successfully() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut io = Io::default();
    let result = leyline::run(
        [OsString::from("leyline")],
        Env {
            xdg: Some(temp.path().as_os_str().into()),
            home: None,
        },
        &mut io,
    );
    assert_eq!(result.exit_code(), 0);
    assert!(io.stdout.is_empty());
    assert!(io.stderr.is_empty());
    assert_eq!(io.logging, 1);
}

#[test]
fn invalid_environment_has_stable_category_and_exit_code() {
    let mut io = Io::default();
    let result = leyline::run(
        [OsString::from("leyline")],
        Env {
            xdg: None,
            home: None,
        },
        &mut io,
    );
    assert_eq!(result.exit_code(), 2);
    assert!(io.stderr.contains("environment error"));
    assert!(!io.stderr.contains("panicked"));
}

#[test]
fn invalid_config_reports_path_and_reason() {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = temp.path().join("leyline");
    std::fs::create_dir(&dir).expect("config dir");
    std::fs::write(dir.join("config.toml"), "[font]\nsize = 80\n").expect("config");
    let mut io = Io::default();
    let result = leyline::run(
        [OsString::from("leyline")],
        Env {
            xdg: Some(temp.path().as_os_str().into()),
            home: None,
        },
        &mut io,
    );
    assert_eq!(result.exit_code(), 2);
    assert!(io.stderr.contains("configuration error"));
    assert!(io.stderr.contains("font.size"));
    assert!(io.stderr.contains("config.toml"));
}

#![forbid(unsafe_code)]

pub mod app;
pub mod bell;
pub mod cli;
pub mod clipboard;
pub mod config;
pub mod desktop;
pub mod diagnostics;
pub mod frame_composer;
pub mod input;
pub mod interaction;
pub mod layout;
pub mod logging;
pub mod notification;
pub mod perf;
pub mod search;
pub mod security;
pub mod selection;
pub mod session;
pub mod signal;
pub mod sound;
pub mod tab;
pub mod terminal;
pub mod terminfo;
pub mod ui_runtime;
pub mod unicode_layout;
pub mod window;

use std::{ffi::OsString, sync::Arc};

use app::{
    AppBuilder,
    event::ShutdownReason,
    runtime::{AppRuntimeBuilder, CountingWake},
};
use cli::{DoctorOperation, Operation, ParseOutcome, TerminfoOperation, Verbosity};
use config::{ConfigEnvironment, ConfigSource, FileConfigSource};
use diagnostics::{ClassifiedError, ErrorCategory, render_error};

pub trait ProcessIo {
    fn stdout(&mut self, text: &str);
    fn stderr(&mut self, text: &str);
    fn install_panic_hook(&mut self);
    /// Installs logging after CLI and configuration processing.
    ///
    /// # Errors
    /// Returns [`logging::LoggingError`] when process logging cannot be initialized.
    fn initialize_logging(&mut self, verbosity: Verbosity) -> Result<(), logging::LoggingError>;
    /// Selects the real desktop loop. Test/process adapters remain headless by default.
    fn graphical_session(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunOutcome {
    exit_code: u8,
}
impl RunOutcome {
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        self.exit_code
    }
}

pub fn run(
    args: impl IntoIterator<Item = OsString>,
    environment: impl ConfigEnvironment,
    io: &mut impl ProcessIo,
) -> RunOutcome {
    let perf_output_guard = perf::initialize_from_env();
    if perf_output_guard.is_some() {
        leyline_pty::set_write_latency_observer(|elapsed| {
            perf::record_duration(perf::PerfStage::InputToPtyWrite, elapsed);
        });
    }
    let cli = match cli::parse(args) {
        ParseOutcome::Print { text, success } => {
            if success {
                io.stdout(&text);
            } else {
                io.stderr(&text);
            }
            return RunOutcome {
                exit_code: if success {
                    0
                } else {
                    ErrorCategory::Cli.exit_code()
                },
            };
        }
        ParseOutcome::Run(cli) => cli,
    };
    if cli.operation != Operation::Launch {
        return run_management(&cli, io);
    }
    match startup(&cli, environment, io) {
        Ok(ui_runtime::UiExit::Clean) => RunOutcome { exit_code: 0 },
        Ok(ui_runtime::UiExit::Signaled(signal)) => RunOutcome {
            exit_code: signal.exit_code(),
        },
        Err(error) => {
            io.stderr(&render_error(&error, error.verbose));
            RunOutcome {
                exit_code: error.category().exit_code(),
            }
        }
    }
}

fn run_management(cli: &cli::Cli, io: &mut impl ProcessIo) -> RunOutcome {
    let result = match &cli.operation {
        Operation::Launch => unreachable!("launch is handled by startup"),
        Operation::Terminfo(TerminfoOperation::Print) => {
            io.stdout(terminfo::SOURCE);
            return RunOutcome { exit_code: 0 };
        }
        Operation::Terminfo(TerminfoOperation::Check { database }) => {
            terminfo::check(database.as_deref())
        }
        Operation::Terminfo(TerminfoOperation::Install { database }) => {
            terminfo::install(database.as_deref())
        }
        Operation::Terminfo(TerminfoOperation::Uninstall { database }) => {
            terminfo::uninstall(database.as_deref())
        }
        Operation::Doctor(DoctorOperation::Terminfo) => terminfo::doctor(),
        Operation::Doctor(DoctorOperation::Ssh { host, json }) => terminfo::doctor_ssh(host, *json),
    };
    match result {
        Ok(report) => {
            io.stdout(&report);
            RunOutcome { exit_code: 0 }
        }
        Err(error) => {
            io.stderr(&render_error(&error, cli.verbosity != Verbosity::Warn));
            RunOutcome {
                exit_code: error.category().exit_code(),
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn startup(
    cli: &cli::Cli,
    environment: impl ConfigEnvironment,
    io: &mut impl ProcessIo,
) -> Result<ui_runtime::UiExit, StartupError> {
    let source = FileConfigSource::new(environment);
    let location = source.locate().map_err(|source| StartupError {
        source: StartupErrorSource::Config(source),
        verbose: cli.verbosity != Verbosity::Warn,
    })?;
    let mut loaded = source.load(&location).map_err(|source| StartupError {
        source: StartupErrorSource::Config(source),
        verbose: cli.verbosity != Verbosity::Warn,
    })?;
    if let Some(identity) = cli.terminal_identity {
        loaded.effective.terminal.identity = identity;
    }
    if let Some(geometry) = cli.window.geometry {
        loaded.effective.window.columns = geometry.columns;
        loaded.effective.window.lines = geometry.lines;
    }
    if let Some(state) = cli.window.startup_state {
        loaded.effective.window.startup_state = state;
    }
    terminfo::preflight(loaded.effective.terminal.identity).map_err(|source| StartupError {
        source: StartupErrorSource::Terminfo(source),
        verbose: cli.verbosity != Verbosity::Warn,
    })?;
    io.install_panic_hook();
    io.initialize_logging(cli.verbosity)
        .map_err(|source| StartupError {
            source: StartupErrorSource::Logging(source),
            verbose: cli.verbosity != Verbosity::Warn,
        })?;
    if loaded.effective.terminal.identity == terminfo::TerminalIdentity::Xterm256Color {
        tracing::info!(
            category = "terminal_identity",
            identity = "xterm-256color",
            "using best-effort compatibility identity"
        );
    }
    for warning in loaded.warnings {
        io.stderr(&format!(
            "leyline: configuration warning: {}\n",
            warning.message
        ));
    }

    let launch =
        app::LaunchContext::capture(cli.launch_request()).map_err(|source| StartupError {
            source: StartupErrorSource::Launch(source),
            verbose: cli.verbosity != Verbosity::Warn,
        })?;
    let mut app = AppBuilder::new(Arc::new(loaded.effective), launch).build();
    let event_wake = io
        .graphical_session()
        .then(leyline_gfx::EventWake::new)
        .transpose()
        .map_err(|source| StartupError {
            source: StartupErrorSource::Graphics(source.into()),
            verbose: cli.verbosity != Verbosity::Warn,
        })?;
    if let Some(wake) = event_wake {
        return startup_graphical(app, wake, cli.verbosity != Verbosity::Warn);
    }
    let _runtime = AppRuntimeBuilder::new(Arc::new(CountingWake::default()))
        .build()
        .map_err(|source| StartupError {
            source: StartupErrorSource::Runtime(source),
            verbose: cli.verbosity != Verbosity::Warn,
        })?;
    app.start().map_err(|source| StartupError {
        source: StartupErrorSource::App(source),
        verbose: cli.verbosity != Verbosity::Warn,
    })?;
    tracing::info!(
        category = "application",
        module = "startup",
        "application skeleton initialized"
    );
    app.request_shutdown(ShutdownReason::SkeletonComplete)
        .map_err(|source| StartupError {
            source: StartupErrorSource::App(source),
            verbose: cli.verbosity != Verbosity::Warn,
        })?;
    app.stop().map_err(|source| StartupError {
        source: StartupErrorSource::App(source),
        verbose: cli.verbosity != Verbosity::Warn,
    })?;
    Ok(ui_runtime::UiExit::Clean)
}

fn startup_graphical(
    mut app: app::App,
    wake: leyline_gfx::EventWake,
    verbose: bool,
) -> Result<ui_runtime::UiExit, StartupError> {
    let mut relay = signal::SignalRelay::install(wake.clone()).map_err(|source| StartupError {
        source: StartupErrorSource::Signal(source),
        verbose,
    })?;
    let state = relay.state();
    let result = (|| {
        let wake_backend: Arc<dyn app::runtime::WakeBackend> = Arc::new(wake.clone());
        let runtime = AppRuntimeBuilder::new(wake_backend)
            .build()
            .map_err(|source| StartupError {
                source: StartupErrorSource::Runtime(source),
                verbose,
            })?;
        app.start().map_err(|source| StartupError {
            source: StartupErrorSource::App(source),
            verbose,
        })?;
        let start = ui_runtime::DesktopRuntime::initialize(app, runtime, wake, state).map_err(
            |source| StartupError {
                source: StartupErrorSource::Graphics(source),
                verbose,
            },
        )?;
        match start {
            ui_runtime::DesktopRuntimeStart::Ready(runtime) => {
                (*runtime).run().map_err(|source| StartupError {
                    source: StartupErrorSource::Graphics(source),
                    verbose,
                })
            }
            ui_runtime::DesktopRuntimeStart::Signaled(signal) => {
                Ok(ui_runtime::UiExit::Signaled(signal))
            }
        }
    })();
    if let Err(source) = relay.close_and_join() {
        return Err(StartupError {
            source: StartupErrorSource::Signal(source),
            verbose,
        });
    }
    result
}

#[derive(Debug, thiserror::Error)]
#[error("{source}")]
struct StartupError {
    #[source]
    source: StartupErrorSource,
    verbose: bool,
}

#[derive(Debug, thiserror::Error)]
enum StartupErrorSource {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Logging(#[from] logging::LoggingError),
    #[error(transparent)]
    App(#[from] app::AppError),
    #[error(transparent)]
    Launch(#[from] app::LaunchContextError),
    #[error(transparent)]
    Runtime(#[from] app::runtime::RuntimeBuildError),
    #[error(transparent)]
    Signal(#[from] signal::SignalRelayError),
    #[error(transparent)]
    Graphics(#[from] ui_runtime::UiRuntimeError),
    #[error(transparent)]
    Terminfo(#[from] terminfo::TerminfoError),
}

impl ClassifiedError for StartupError {
    fn category(&self) -> ErrorCategory {
        match &self.source {
            StartupErrorSource::Config(error) => error.category(),
            StartupErrorSource::Graphics(error) => error.category(),
            StartupErrorSource::Logging(_)
            | StartupErrorSource::Launch(_)
            | StartupErrorSource::App(_)
            | StartupErrorSource::Runtime(_)
            | StartupErrorSource::Signal(_) => ErrorCategory::Internal,
            StartupErrorSource::Terminfo(error) => error.category(),
        }
    }
}

#![forbid(unsafe_code)]

pub mod app;
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
pub mod security;
pub mod selection;
pub mod session;
pub mod terminal;
pub mod ui_runtime;

use std::{ffi::OsString, sync::Arc};

use app::{
    AppBuilder,
    event::ShutdownReason,
    runtime::{AppRuntimeBuilder, CountingWake},
};
use cli::{ParseOutcome, Verbosity};
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
    match startup(&cli, environment, io) {
        Ok(()) => RunOutcome { exit_code: 0 },
        Err(error) => {
            io.stderr(&render_error(&error, error.verbose));
            RunOutcome {
                exit_code: error.category().exit_code(),
            }
        }
    }
}

fn startup(
    cli: &cli::Cli,
    environment: impl ConfigEnvironment,
    io: &mut impl ProcessIo,
) -> Result<(), StartupError> {
    let source = FileConfigSource::new(environment);
    let location = source.locate().map_err(|source| StartupError {
        source: StartupErrorSource::Config(source),
        verbose: cli.verbosity != Verbosity::Warn,
    })?;
    let loaded = source.load(&location).map_err(|source| StartupError {
        source: StartupErrorSource::Config(source),
        verbose: cli.verbosity != Verbosity::Warn,
    })?;
    io.install_panic_hook();
    io.initialize_logging(cli.verbosity)
        .map_err(|source| StartupError {
            source: StartupErrorSource::Logging(source),
            verbose: cli.verbosity != Verbosity::Warn,
        })?;
    for warning in loaded.warnings {
        io.stderr(&format!(
            "leyline: configuration warning: {}\n",
            warning.message
        ));
    }

    let mut app = AppBuilder::new(Arc::new(loaded.effective), cli.launch_request()).build();
    let event_wake = io
        .graphical_session()
        .then(leyline_gfx::EventWake::new)
        .transpose()
        .map_err(|source| StartupError {
            source: StartupErrorSource::Graphics(source.into()),
            verbose: cli.verbosity != Verbosity::Warn,
        })?;
    let wake_backend: Arc<dyn app::runtime::WakeBackend> = event_wake.as_ref().map_or_else(
        || Arc::new(CountingWake::default()) as Arc<dyn app::runtime::WakeBackend>,
        |wake| Arc::new(wake.clone()) as Arc<dyn app::runtime::WakeBackend>,
    );
    let runtime = AppRuntimeBuilder::new(wake_backend)
        .build()
        .map_err(|source| StartupError {
            source: StartupErrorSource::Runtime(source),
            verbose: cli.verbosity != Verbosity::Warn,
        })?;
    app.start().map_err(|source| StartupError {
        source: StartupErrorSource::App(source),
        verbose: cli.verbosity != Verbosity::Warn,
    })?;
    if io.graphical_session() {
        return ui_runtime::UiRuntime::new(
            app,
            runtime,
            event_wake.expect("graphical wake exists"),
        )
        .map_err(|source| StartupError {
            source: StartupErrorSource::Graphics(source),
            verbose: cli.verbosity != Verbosity::Warn,
        })?
        .run()
        .map_err(|source| StartupError {
            source: StartupErrorSource::Graphics(source),
            verbose: cli.verbosity != Verbosity::Warn,
        });
    }
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
    Ok(())
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
    Runtime(#[from] app::runtime::RuntimeBuildError),
    #[error(transparent)]
    Graphics(#[from] ui_runtime::UiRuntimeError),
}

impl ClassifiedError for StartupError {
    fn category(&self) -> ErrorCategory {
        match &self.source {
            StartupErrorSource::Config(error) => error.category(),
            StartupErrorSource::Graphics(error) => error.category(),
            StartupErrorSource::Logging(_)
            | StartupErrorSource::App(_)
            | StartupErrorSource::Runtime(_) => ErrorCategory::Internal,
        }
    }
}

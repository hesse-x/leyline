pub mod event;
pub mod runtime;

use std::{path::PathBuf, rc::Rc, sync::Arc};

use crate::{
    cli::LaunchRequest,
    config::EffectiveConfig,
    diagnostics::{ClassifiedError, ErrorCategory},
    terminal::cwd::{LocalIdentity, valid_absolute_env_path},
};
use event::{AppEvent, ShutdownReason};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    Starting,
    Running,
    ShuttingDown(ShutdownReason),
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppAction {
    Continue,
    BeginShutdown,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownTransition {
    Started,
    AlreadyShuttingDown,
}

pub struct App {
    config: Arc<EffectiveConfig>,
    launch: LaunchContext,
    lifecycle: Lifecycle,
    // Rc makes the coordinator deliberately !Send and pins mutable ownership to its creating thread.
    _main_thread: Rc<()>,
}

impl App {
    #[must_use]
    pub fn lifecycle(&self) -> &Lifecycle {
        &self.lifecycle
    }
    #[must_use]
    pub fn config(&self) -> &EffectiveConfig {
        &self.config
    }
    #[must_use]
    pub const fn launch(&self) -> &LaunchRequest {
        &self.launch.request
    }
    #[must_use]
    pub const fn launch_context(&self) -> &LaunchContext {
        &self.launch
    }

    /// Moves the coordinator from starting to running.
    ///
    /// # Errors
    /// Returns [`AppError`] when the lifecycle is not `Starting`.
    pub fn start(&mut self) -> Result<(), AppError> {
        if self.lifecycle != Lifecycle::Starting {
            return Err(self.invalid_transition("start"));
        }
        self.lifecycle = Lifecycle::Running;
        tracing::debug!(
            category = "application",
            module = "app",
            lifecycle = "running"
        );
        Ok(())
    }

    /// Applies one finite event to the application state.
    ///
    /// # Errors
    /// Returns [`AppError`] when the event is invalid for the current lifecycle.
    pub fn handle_event(&mut self, event: AppEvent) -> Result<AppAction, AppError> {
        match event {
            AppEvent::ShutdownRequested(reason) => {
                let transition = self.request_shutdown(reason)?;
                Ok(if transition == ShutdownTransition::Started {
                    AppAction::BeginShutdown
                } else {
                    AppAction::Continue
                })
            }
            AppEvent::Pty(_)
                if matches!(
                    self.lifecycle,
                    Lifecycle::Running | Lifecycle::ShuttingDown(_)
                ) =>
            {
                Ok(AppAction::Continue)
            }
            AppEvent::Pty(_) => Err(self.invalid_transition("handle PTY event")),
        }
    }

    /// Starts an idempotent shutdown while preserving the first reason.
    ///
    /// # Errors
    /// Reserved for lifecycle invariant violations.
    pub fn request_shutdown(
        &mut self,
        reason: ShutdownReason,
    ) -> Result<ShutdownTransition, AppError> {
        match self.lifecycle {
            Lifecycle::Starting | Lifecycle::Running => {
                self.lifecycle = Lifecycle::ShuttingDown(reason);
                Ok(ShutdownTransition::Started)
            }
            Lifecycle::ShuttingDown(_) | Lifecycle::Stopped => {
                Ok(ShutdownTransition::AlreadyShuttingDown)
            }
        }
    }

    /// Completes a shutdown transition.
    ///
    /// # Errors
    /// Returns [`AppError`] unless shutdown has already started.
    pub fn stop(&mut self) -> Result<(), AppError> {
        if !matches!(self.lifecycle, Lifecycle::ShuttingDown(_)) {
            return Err(self.invalid_transition("stop"));
        }
        self.lifecycle = Lifecycle::Stopped;
        Ok(())
    }

    fn invalid_transition(&mut self, operation: &'static str) -> AppError {
        let from = format!("{:?}", self.lifecycle);
        if !matches!(
            self.lifecycle,
            Lifecycle::Stopped | Lifecycle::ShuttingDown(_)
        ) {
            self.lifecycle = Lifecycle::ShuttingDown(ShutdownReason::StartupFailure);
        }
        AppError::InvalidTransition { operation, from }
    }
}

pub struct AppBuilder {
    config: Arc<EffectiveConfig>,
    launch: LaunchContext,
}

impl AppBuilder {
    #[must_use]
    pub const fn new(config: Arc<EffectiveConfig>, launch: LaunchContext) -> Self {
        Self { config, launch }
    }
    #[must_use]
    pub fn build(self) -> App {
        App {
            config: self.config,
            launch: self.launch,
            lifecycle: Lifecycle::Starting,
            _main_thread: Rc::new(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchContext {
    pub request: LaunchRequest,
    pub base_cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub local_identity: LocalIdentity,
}

impl LaunchContext {
    /// Captures immutable process launch metadata before the first session starts.
    ///
    /// # Errors
    /// Returns an error when the process startup directory cannot be captured.
    pub fn capture(request: LaunchRequest) -> Result<Self, LaunchContextError> {
        let base_cwd = std::env::current_dir().map_err(LaunchContextError::CurrentDirectory)?;
        if !base_cwd.is_absolute() {
            return Err(LaunchContextError::RelativeCurrentDirectory);
        }
        Ok(Self {
            request,
            base_cwd,
            home: valid_absolute_env_path(std::env::var_os("HOME")),
            local_identity: LocalIdentity::capture(),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(request: LaunchRequest) -> Self {
        Self {
            request,
            base_cwd: PathBuf::from("/tmp"),
            home: Some(PathBuf::from("/tmp")),
            local_identity: LocalIdentity::new(Some("localhost".into())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchContextError {
    #[error("cannot determine startup working directory: {0}")]
    CurrentDirectory(std::io::Error),
    #[error("startup working directory is not absolute")]
    RelativeCurrentDirectory,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("cannot {operation} while lifecycle is {from}")]
    InvalidTransition {
        operation: &'static str,
        from: String,
    },
}

impl ClassifiedError for AppError {
    fn category(&self) -> ErrorCategory {
        ErrorCategory::Internal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifecycle_is_forward_only_and_shutdown_is_idempotent() {
        let mut app = AppBuilder::new(
            Arc::new(EffectiveConfig::default()),
            LaunchContext::for_test(LaunchRequest::DefaultShell),
        )
        .build();
        app.start().expect("start");
        assert_eq!(
            app.request_shutdown(ShutdownReason::UserRequested)
                .expect("shutdown"),
            ShutdownTransition::Started
        );
        assert_eq!(
            app.request_shutdown(ShutdownReason::PlatformFailure)
                .expect("shutdown"),
            ShutdownTransition::AlreadyShuttingDown
        );
        assert_eq!(
            app.lifecycle(),
            &Lifecycle::ShuttingDown(ShutdownReason::UserRequested)
        );
        app.stop().expect("stop");
    }
}

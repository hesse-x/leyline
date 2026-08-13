use leyline_gfx::{
    EventWake, GfxError, GfxInitError, GfxOptions, GfxRuntime, LinearColor, PlatformEvent,
    RenderOutcome, WakeError,
};

use crate::{
    app::{
        App, AppAction,
        event::ShutdownReason,
        runtime::{AppRuntime, WakeBackend},
    },
    diagnostics::{ClassifiedError, ErrorCategory},
};

pub struct UiRuntime {
    app: App,
    app_runtime: AppRuntime,
    gfx: GfxRuntime,
    wake: EventWake,
}

impl UiRuntime {
    /// Builds the single UI-thread composition root.
    ///
    /// # Errors
    /// Returns a typed graphics initialization failure.
    pub fn new(app: App, app_runtime: AppRuntime, wake: EventWake) -> Result<Self, UiRuntimeError> {
        let clear = LinearColor::from_srgba8(app.config().colors.background.0);
        let gfx = GfxRuntime::new(&GfxOptions {
            clear,
            ..GfxOptions::default()
        })?;
        Ok(Self {
            app,
            app_runtime,
            gfx,
            wake,
        })
    }

    /// Runs the demand-driven window loop until the compositor requests close.
    ///
    /// # Errors
    /// Returns a typed platform, renderer, or application failure.
    pub fn run(mut self) -> Result<(), UiRuntimeError> {
        loop {
            let mut events = Vec::new();
            self.gfx.dispatch_pending(&mut events)?;
            for event in events {
                if event == PlatformEvent::CloseRequested {
                    self.handle_app_event(crate::app::event::AppEvent::ShutdownRequested(
                        ShutdownReason::UserRequested,
                    ))?;
                }
            }
            let mut app_events = Vec::new();
            self.app_runtime
                .inbox()
                .drain_round(|event| app_events.push(event));
            for event in app_events {
                self.handle_app_event(event)?;
            }
            if self.gfx.close_requested() {
                break;
            }
            let timeout = match self.gfx.try_render()? {
                RenderOutcome::Deferred => Some(GfxRuntime::retry_delay()),
                RenderOutcome::Rendered | RenderOutcome::WaitingForFrame | RenderOutcome::Idle => {
                    None
                }
            };
            if self.app_runtime.inbox().prepare_to_wait() {
                self.gfx.poll_wait(Some(self.wake.as_fd()), timeout)?;
                self.wake.drain()?;
            }
        }
        self.app.stop()?;
        Ok(())
    }

    fn handle_app_event(
        &mut self,
        event: crate::app::event::AppEvent,
    ) -> Result<(), UiRuntimeError> {
        match self.app.handle_event(event)? {
            AppAction::BeginShutdown => {
                self.app_runtime.fast_cancel();
                self.gfx.apply(leyline_gfx::GfxCommand::RequestClose)?;
            }
            AppAction::Continue | AppAction::Stop => {}
        }
        Ok(())
    }
}

impl WakeBackend for EventWake {
    fn wake(&self) {
        if let Err(error) = self.signal() {
            tracing::error!(category = "runtime", %error, "cannot signal UI eventfd");
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UiRuntimeError {
    #[error(transparent)]
    Init(#[from] GfxInitError),
    #[error(transparent)]
    Graphics(#[from] GfxError),
    #[error(transparent)]
    App(#[from] crate::app::AppError),
    #[error(transparent)]
    Wake(#[from] WakeError),
}

impl ClassifiedError for UiRuntimeError {
    fn category(&self) -> ErrorCategory {
        match self {
            Self::Init(GfxInitError::Environment(_)) => ErrorCategory::Environment,
            Self::Init(GfxInitError::Platform(_)) => ErrorCategory::Platform,
            Self::Init(GfxInitError::Device(_)) => ErrorCategory::Renderer,
            Self::Graphics(GfxError::Internal(_)) | Self::App(_) | Self::Wake(_) => {
                ErrorCategory::Internal
            }
            Self::Graphics(GfxError::Platform(_) | GfxError::Renderer(_)) => {
                ErrorCategory::Environment
            }
        }
    }
}

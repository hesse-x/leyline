use leyline_gfx::{
    EventWake, GfxError, GfxInitError, GfxOptions, GfxRuntime, LinearColor, PlatformEvent,
    RenderOutcome, WakeError,
};
use leyline_text::{FontRequest, TextSystem};

use crate::{
    app::{
        App, AppAction,
        event::ShutdownReason,
        runtime::{AppRuntime, WakeBackend},
    },
    diagnostics::{ClassifiedError, ErrorCategory},
    frame_composer::{SelectionOverlay, compose},
    layout::GridLayout,
    session::{SessionAction, TerminalSession},
};

pub struct UiRuntime {
    app: App,
    app_runtime: AppRuntime,
    gfx: GfxRuntime,
    wake: EventWake,
    session: TerminalSession,
    text: TextSystem,
    layout: GridLayout,
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
        let request = FontRequest::from_points(
            app.config().font.family.clone(),
            app.config().font.size,
            gfx.scale().0,
            app.config().font.ligatures,
        )?;
        let text = TextSystem::new(request)?;
        let layout = GridLayout::calculate(
            gfx.logical_size(),
            gfx.scale(),
            [app.config().window.padding_x, app.config().window.padding_y],
            text.metrics(),
            text.generation(),
        )?;
        let initial_size = layout.grid;
        let session =
            TerminalSession::start(app.launch(), app.config(), initial_size, &app_runtime)?;
        Ok(Self {
            app,
            app_runtime,
            gfx,
            wake,
            session,
            text,
            layout,
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
                match event {
                    PlatformEvent::CloseRequested => {
                        self.handle_app_event(crate::app::event::AppEvent::ShutdownRequested(
                            ShutdownReason::UserRequested,
                        ))?;
                    }
                    PlatformEvent::Configured {
                        logical_size,
                        scale,
                        ..
                    } => {
                        self.reconfigure_layout(logical_size, scale)?;
                    }
                    PlatformEvent::ScaleChanged { scale } => {
                        self.reconfigure_layout(self.gfx.logical_size(), scale)?;
                    }
                    PlatformEvent::FrameReady
                    | PlatformEvent::SurfaceSuspended
                    | PlatformEvent::SurfaceResumed => {}
                }
            }
            let mut app_events = Vec::new();
            self.app_runtime
                .inbox()
                .drain_round(|event| app_events.push(event));
            for event in app_events {
                self.handle_app_event(event)?;
            }
            if let Some(snapshot) = self.session.end_drain_round()? {
                let scene = compose(
                    &mut self.text,
                    &snapshot,
                    &SelectionOverlay::default(),
                    &self.layout,
                    &self.app.config().colors,
                    self.app.config().cursor.style,
                )?;
                self.gfx.apply(leyline_gfx::GfxCommand::SetScene(scene))?;
            }
            if let Some(title) = self.session.take_title() {
                self.gfx
                    .apply(leyline_gfx::GfxCommand::SetTitle(title.to_string()))?;
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

    fn reconfigure_layout(
        &mut self,
        logical: leyline_gfx::LogicalSize,
        scale: leyline_gfx::Scale120,
    ) -> Result<(), UiRuntimeError> {
        let request = FontRequest::from_points(
            self.app.config().font.family.clone(),
            self.app.config().font.size,
            scale.0,
            self.app.config().font.ligatures,
        )?;
        self.text.configure(request)?;
        let layout = GridLayout::calculate(
            logical,
            scale,
            [
                self.app.config().window.padding_x,
                self.app.config().window.padding_y,
            ],
            self.text.metrics(),
            self.text.generation(),
        )?;
        let grid_changed = self.layout.grid != layout.grid;
        if grid_changed {
            self.session.resize(layout.grid)?;
        }
        self.layout = layout;
        if !grid_changed && let Some(snapshot) = self.session.latest_snapshot().cloned() {
            let scene = compose(
                &mut self.text,
                &snapshot,
                &SelectionOverlay::default(),
                &self.layout,
                &self.app.config().colors,
                self.app.config().cursor.style,
            )?;
            self.gfx.apply(leyline_gfx::GfxCommand::SetScene(scene))?;
        }
        Ok(())
    }

    fn handle_app_event(
        &mut self,
        event: crate::app::event::AppEvent,
    ) -> Result<(), UiRuntimeError> {
        if let crate::app::event::AppEvent::Pty(pty) = &event {
            match self.session.handle_pty_event(pty.clone())? {
                SessionAction::Completed => {
                    return self.handle_app_event(crate::app::event::AppEvent::ShutdownRequested(
                        ShutdownReason::ChildExited,
                    ));
                }
                SessionAction::Continue | SessionAction::Held => {}
                SessionAction::Failed => {
                    return self.handle_app_event(crate::app::event::AppEvent::ShutdownRequested(
                        ShutdownReason::PlatformFailure,
                    ));
                }
            }
        }
        match self.app.handle_event(event)? {
            AppAction::BeginShutdown => {
                self.app_runtime.fast_cancel();
                self.session.begin_shutdown();
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
    #[error(transparent)]
    SessionStart(#[from] crate::session::SessionStartError),
    #[error(transparent)]
    Session(#[from] crate::session::SessionError),
    #[error("cannot calculate terminal grid: {0}")]
    Grid(String),
    #[error(transparent)]
    Text(#[from] leyline_text::TextError),
    #[error(transparent)]
    Layout(#[from] crate::layout::LayoutError),
    #[error(transparent)]
    Compose(#[from] crate::frame_composer::ComposeError),
}

impl ClassifiedError for UiRuntimeError {
    fn category(&self) -> ErrorCategory {
        match self {
            Self::Init(GfxInitError::Environment(_))
            | Self::SessionStart(_)
            | Self::Graphics(GfxError::Platform(_) | GfxError::Renderer(_)) => {
                ErrorCategory::Environment
            }
            Self::Init(GfxInitError::Platform(_)) => ErrorCategory::Platform,
            Self::Init(GfxInitError::Device(_)) => ErrorCategory::Renderer,
            Self::Graphics(GfxError::Internal(_))
            | Self::App(_)
            | Self::Wake(_)
            | Self::Session(_)
            | Self::Grid(_)
            | Self::Text(_)
            | Self::Layout(_)
            | Self::Compose(_) => ErrorCategory::Internal,
        }
    }
}

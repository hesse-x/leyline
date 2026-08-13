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
    frame_composer::compose,
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
    font_size: f64,
    reset_font_size: f64,
    modifiers: leyline_gfx::ModifiersState,
    selecting: bool,
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
        let reset_font_size = app.config().font.size;
        Ok(Self {
            app,
            app_runtime,
            gfx,
            wake,
            session,
            text,
            layout,
            font_size: reset_font_size,
            reset_font_size,
            modifiers: leyline_gfx::ModifiersState::default(),
            selecting: false,
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
                    PlatformEvent::KeyboardFocus { focused, .. } => {
                        self.session.focus_changed(focused)?;
                    }
                    PlatformEvent::Key(key) => self.handle_key(key)?,
                    PlatformEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers,
                    PlatformEvent::Pointer(pointer) => self.handle_pointer(pointer)?,
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
                    &self.session.selection_overlay(snapshot.generation),
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
            self.font_size,
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
                &self.session.selection_overlay(snapshot.generation),
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

    fn handle_key(&mut self, key: leyline_gfx::KeyInput) -> Result<(), UiRuntimeError> {
        if key.state == leyline_gfx::KeyState::Released {
            return Ok(());
        }
        let modifiers = crate::terminal::Modifiers {
            shift: key.modifiers.shift,
            control: key.modifiers.control,
            alt: key.modifiers.alt,
            super_key: key.modifiers.super_key,
        };
        if let Some(action) = self.resolve_shortcut(&key) {
            self.execute_action(action)?;
            return Ok(());
        }
        let terminal_key = match key.keysym_name.as_deref() {
            Some("BackSpace") => Some(crate::terminal::TerminalKey::Backspace),
            Some("Tab" | "ISO_Left_Tab") => Some(crate::terminal::TerminalKey::Tab),
            Some("Return" | "KP_Enter") => Some(crate::terminal::TerminalKey::Enter),
            Some("Escape") => Some(crate::terminal::TerminalKey::Escape),
            Some("Up") => Some(crate::terminal::TerminalKey::Up),
            Some("Down") => Some(crate::terminal::TerminalKey::Down),
            Some("Left") => Some(crate::terminal::TerminalKey::Left),
            Some("Right") => Some(crate::terminal::TerminalKey::Right),
            Some("Home") => Some(crate::terminal::TerminalKey::Home),
            Some("End") => Some(crate::terminal::TerminalKey::End),
            Some("Insert") => Some(crate::terminal::TerminalKey::Insert),
            Some("Delete") => Some(crate::terminal::TerminalKey::Delete),
            Some("Page_Up") => Some(crate::terminal::TerminalKey::PageUp),
            Some("Page_Down") => Some(crate::terminal::TerminalKey::PageDown),
            Some(name) if name.len() <= 3 && name.starts_with('F') => name[1..]
                .parse()
                .ok()
                .map(crate::terminal::TerminalKey::Function),
            _ => None,
        };
        if let Some(key) = terminal_key {
            self.session.input_key(key, modifiers)?;
        } else if let Some(text) = key.utf8 {
            if modifiers.control || modifiers.alt {
                if let Some(ch) = text.chars().next() {
                    self.session
                        .input_key(crate::terminal::TerminalKey::Char(ch), modifiers)?;
                }
            } else {
                self.session.commit_text(&text)?;
            }
        }
        Ok(())
    }

    fn resolve_shortcut(&self, key: &leyline_gfx::KeyInput) -> Option<crate::config::Action> {
        use crate::config::Modifier;
        let normalized_key = match key.keysym_name.as_deref()? {
            "equal" if key.modifiers.shift => "Plus",
            "minus" => "Minus",
            "Page_Up" => "PageUp",
            "Page_Down" => "PageDown",
            name => name,
        };
        self.app
            .config()
            .keybindings
            .iter()
            .rev()
            .find(|binding| {
                binding.key.eq_ignore_ascii_case(normalized_key)
                    && binding.mods.contains(&Modifier::Control) == key.modifiers.control
                    && binding.mods.contains(&Modifier::Shift) == key.modifiers.shift
                    && binding.mods.contains(&Modifier::Alt) == key.modifiers.alt
                    && binding.mods.contains(&Modifier::Super) == key.modifiers.super_key
            })
            .map(|binding| binding.action)
    }

    fn execute_action(&mut self, action: crate::config::Action) -> Result<(), UiRuntimeError> {
        use crate::config::Action;
        match action {
            Action::IncreaseFontSize => {
                self.font_size = (self.font_size + 1.0).min(72.0);
                self.reconfigure_layout(self.gfx.logical_size(), self.gfx.scale())?;
            }
            Action::DecreaseFontSize => {
                self.font_size = (self.font_size - 1.0).max(6.0);
                self.reconfigure_layout(self.gfx.logical_size(), self.gfx.scale())?;
            }
            Action::ResetFontSize => {
                self.font_size = self.reset_font_size;
                self.reconfigure_layout(self.gfx.logical_size(), self.gfx.scale())?;
            }
            Action::ScrollPageUp => self.session.scroll(
                i32::try_from(self.layout.grid.lines().saturating_sub(1)).unwrap_or(i32::MAX),
            )?,
            Action::ScrollPageDown => self.session.scroll(
                -i32::try_from(self.layout.grid.lines().saturating_sub(1)).unwrap_or(i32::MAX),
            )?,
            Action::Copy | Action::Paste => {
                tracing::warn!(
                    ?action,
                    "desktop action requires an active selection/data offer"
                );
            }
        }
        Ok(())
    }

    fn handle_pointer(&mut self, event: leyline_gfx::PointerInput) -> Result<(), UiRuntimeError> {
        let scale = f64::from(self.gfx.scale().0) / 120.0;
        if !event.position.0.is_finite()
            || !event.position.1.is_finite()
            || event.position.0 < 0.0
            || event.position.1 < 0.0
        {
            return Ok(());
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pixel = [
            (event.position.0 * scale).floor() as u32,
            (event.position.1 * scale).floor() as u32,
        ];
        let Some([column, line]) = self.layout.cell_at_pixel(pixel) else {
            return Ok(());
        };
        let point = crate::terminal::SelectionPoint { column, line };
        let modifiers = crate::terminal::Modifiers {
            shift: self.modifiers.shift,
            control: self.modifiers.control,
            alt: self.modifiers.alt,
            super_key: self.modifiers.super_key,
        };
        match event.kind {
            leyline_gfx::PointerKind::Press { button: 0x110, .. } => {
                if !self.session.pointer_report(
                    crate::terminal::MouseButton::Left,
                    crate::terminal::ButtonState::Pressed,
                    point,
                    modifiers,
                )? {
                    self.session.start_selection(point)?;
                    self.selecting = true;
                }
            }
            leyline_gfx::PointerKind::Release { button: 0x110, .. } => {
                if !self.session.pointer_report(
                    crate::terminal::MouseButton::Left,
                    crate::terminal::ButtonState::Released,
                    point,
                    modifiers,
                )? && self.selecting
                {
                    self.session.update_selection(point)?;
                }
                self.selecting = false;
            }
            leyline_gfx::PointerKind::Motion { .. } if self.selecting => {
                self.session.update_selection(point)?;
            }
            leyline_gfx::PointerKind::Axis { vertical_120, .. } if vertical_120 != 0 => {
                let button = if vertical_120 < 0 {
                    crate::terminal::MouseButton::WheelUp
                } else {
                    crate::terminal::MouseButton::WheelDown
                };
                if !self.session.pointer_report(
                    button,
                    crate::terminal::ButtonState::Pressed,
                    point,
                    modifiers,
                )? {
                    self.session.scroll((-vertical_120 / 120).clamp(-12, 12))?;
                }
            }
            leyline_gfx::PointerKind::Leave { .. } => self.selecting = false,
            _ => {}
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

use std::{
    num::NonZeroU8,
    sync::Arc,
    time::{Duration, Instant},
};

use leyline_gfx::{
    EventWake, GfxError, GfxInitError, GfxOptions, GfxRuntime, LinearColor, PlatformEvent,
    RenderOutcome, WakeError,
};
use leyline_pty::SpawnDirectory;
use leyline_text::{AntialiasPreference, FontRequest, HintingPreference, TextSystem};

use crate::{
    app::{
        App, AppAction,
        event::ShutdownReason,
        runtime::{AppRuntime, AppRuntimeBuilder, WakeBackend},
    },
    diagnostics::{ClassifiedError, ErrorCategory},
    frame_composer::{FrameOverlays, compose},
    interaction::{ClickTracker, ImeState, LinkCandidate, ScrollbarController, ScrollbarGeometry},
    layout::{ContentInsets, GridLayout, TerminalGeometry},
    session::{SessionAction, SessionReplyRequest, ShutdownPoll, TerminalSession},
    tab::TabManager,
};

#[allow(clippy::struct_excessive_bools)]
pub struct UiRuntime {
    state: RuntimeState,
    app: App,
    gfx: GfxRuntime,
    wake: EventWake,
    tabs: TabManager,
    wake_backend: Arc<dyn WakeBackend>,
    text: TextSystem,
    tab_text: TextSystem,
    layout: GridLayout,
    layout_generation: u64,
    text_scale: leyline_gfx::Scale120,
    resize_settle_deadline: Option<Instant>,
    font_size: f64,
    reset_font_size: f64,
    modifiers: leyline_gfx::ModifiersState,
    terminal_control_gesture: bool,
    keyboard_focused: bool,
    cursor_blink_visible: bool,
    cursor_blink_deadline: Option<Instant>,
    selecting: bool,
    selection_point: Option<crate::terminal::SelectionPoint>,
    selection_kind: Option<crate::terminal::SelectionKind>,
    selection_dragged: bool,
    click_tracker: ClickTracker,
    drag_scroll: Option<DragScroll>,
    link_candidate: Option<LinkCandidate>,
    desktop_launcher: crate::desktop::DesktopLauncher,
    ime: ImeState,
    ime_rectangle: Option<leyline_gfx::TextInputRectangle>,
    clipboard_workers: crate::clipboard::TransferWorkers,
    selection: crate::selection::SelectionController,
    last_input_serial: Option<leyline_gfx::InputSerial>,
    wheel_remainder_120: i32,
    scrollbar: ScrollbarController,
    tab_bar: crate::tab::TabBarPresentation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeState {
    Running,
    FatalPendingExit,
}

#[derive(Clone, Copy, Debug)]
struct DragScroll {
    direction: i32,
    point: crate::terminal::SelectionPoint,
    deadline: Instant,
}

const DRAG_SCROLL_INTERVAL: Duration = Duration::from_millis(50);
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(25);
const RESIZE_SETTLE_INTERVAL: Duration = Duration::from_millis(50);
const CLIPBOARD_RESULT_BUDGET: usize = 2;

impl UiRuntime {
    fn active_session(&self) -> &TerminalSession {
        &self
            .tabs
            .active()
            .expect("running UI has an active tab")
            .session
    }

    fn active_session_mut(&mut self) -> &mut TerminalSession {
        &mut self
            .tabs
            .active_mut()
            .expect("running UI has an active tab")
            .session
    }

    #[allow(clippy::too_many_lines)]
    fn drain_sessions(&mut self) -> Result<(), UiRuntimeError> {
        const WINDOW_EVENT_BUDGET: usize = 64;
        const WINDOW_BYTE_BUDGET: usize = 1024 * 1024;
        const WINDOW_TIME_BUDGET: Duration = Duration::from_millis(2);
        let started = Instant::now();
        let mut events = 0_usize;
        let mut bytes = 0_usize;
        let mut completed = Vec::new();
        let fallback_title = launch_title(self.app.launch());
        let local_identity = self.app.launch_context().local_identity.clone();
        let geometry = self.layout.terminal_geometry(self.layout_generation);
        let foreground = self.app.config().colors.foreground.0;
        let background = self.app.config().colors.background.0;
        for id in self.tabs.drain_order() {
            let is_active = Some(id) == self.tabs.active_id();
            let mut incoming = Vec::new();
            let result = self
                .tabs
                .get_mut(id)
                .expect("drain id exists")
                .runtime
                .inbox()
                .drain_round_limited(
                    WINDOW_EVENT_BUDGET.saturating_sub(events),
                    WINDOW_BYTE_BUDGET.saturating_sub(bytes),
                    |event| incoming.push(event),
                );
            events = events.saturating_add(result.control + result.bulk);
            bytes = bytes.saturating_add(result.bulk_bytes);
            for event in incoming {
                let crate::app::event::AppEvent::Pty(pty) = event else {
                    continue;
                };
                let action = match self
                    .tabs
                    .get_mut(id)
                    .expect("drain id exists")
                    .session
                    .handle_pty_event(pty)
                {
                    Ok(action) => action,
                    Err(error) => {
                        let tab = self.tabs.get_mut(id).expect("drain id exists");
                        tab.session.mark_failed();
                        tab.runtime.fast_cancel();
                        tracing::warn!(category = "tab_session_failed", session_id = id.get(), %error, "tab session failed");
                        continue;
                    }
                };
                if id != self.tabs.active_id().expect("active tab") {
                    self.tabs.get_mut(id).expect("drain id exists").unread = true;
                }
                if matches!(action, SessionAction::Completed) {
                    completed.push(id);
                }
            }
            let tab = self.tabs.get_mut(id).expect("drain id exists");
            for request in tab.session.take_reply_requests() {
                match request {
                    SessionReplyRequest::Bytes(reply) => {
                        tab.session.answer_reply(reply, false)?;
                    }
                    SessionReplyRequest::Query(query) => {
                        if tab.session.grid() != geometry.grid {
                            tab.session.reject_query_reply();
                        } else if let Some(reply) =
                            format_terminal_query(query, geometry, foreground, background)
                        {
                            tab.session.answer_reply(reply, true)?;
                        } else {
                            tab.session.reject_query_reply();
                        }
                    }
                }
            }
            tab.session.reset_reply_budget();
            tab.session.finish_io_round()?;
            let cwd_report = tab.session.take_cwd_report();
            if let Some(report) = cwd_report {
                match self.tabs.apply_cwd_report(id, report, &local_identity) {
                    Some(Ok(())) => tracing::debug!(
                        category = "cwd_report",
                        session_id = id.get(),
                        decision = "accept",
                        "cwd report accepted"
                    ),
                    Some(Err(reason)) => tracing::debug!(
                        category = "cwd_report",
                        session_id = id.get(),
                        decision = "reject",
                        reason = reason.as_str(),
                        "cwd report rejected"
                    ),
                    None => {}
                }
            }
            let tab = self.tabs.get_mut(id).expect("drain id exists");
            if let Some(title) = tab.session.take_title() {
                tab.title = match title {
                    crate::session::SessionTitleDelta::Set(title) => title.to_string(),
                    crate::session::SessionTitleDelta::Reset => fallback_title.clone(),
                };
            }
            if tab.session.take_bell() && !is_active {
                tab.unread = true;
            }
            if events >= WINDOW_EVENT_BUDGET
                || bytes >= WINDOW_BYTE_BUDGET
                || (events > 0 && started.elapsed() >= WINDOW_TIME_BUDGET)
            {
                self.wake.signal()?;
                break;
            }
        }
        for id in completed {
            if self.tabs.is_empty() {
                break;
            }
            if self.tabs.active_id() != Some(id) {
                let _ = self.tabs.activate(id);
            }
            self.close_active_tab(ShutdownReason::ChildExited)?;
        }
        self.tabs.poll_closing(Instant::now())?;
        Ok(())
    }

    fn refresh_active_title(&mut self) -> Result<(), UiRuntimeError> {
        let fallback = launch_title(self.app.launch());
        if let Some(title) = self.active_session_mut().take_title() {
            self.tabs.active_mut().expect("active tab").title = match title {
                crate::session::SessionTitleDelta::Set(title) => title.to_string(),
                crate::session::SessionTitleDelta::Reset => fallback,
            };
        }
        let active = self.tabs.active().expect("active tab");
        let ordinal = self
            .tabs
            .tabs()
            .iter()
            .position(|tab| tab.id == active.id)
            .unwrap_or(0)
            + 1;
        let title = window_title(ordinal, self.tabs.len(), &active.title);
        self.gfx.apply(leyline_gfx::GfxCommand::SetTitle(title))?;
        Ok(())
    }

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
        )?
        .with_rendering(
            text_hinting(app.config().font.hinting),
            text_antialiasing(app.config().font.antialiasing),
        );
        let text = TextSystem::new(request)?;
        let tab_request = tab_font_request(app.config().font.size, gfx.scale().0)?;
        let tab_text = TextSystem::new(tab_request)?;
        if let Some(face) = text.resolved_primary() {
            tracing::info!(
                category = "text_profile",
                requested_family = %crate::diagnostics::escape_diagnostic(&app.config().font.family),
                requested_size_pt = app.config().font.size,
                resolved_family = %crate::diagnostics::escape_diagnostic(&face.family),
                resolved_path = %crate::diagnostics::escape_diagnostic(&face.path),
                face_index = face.index,
                scale_120 = gfx.scale().0,
                hinting = ?text.raster_profile().hinting,
                antialias = ?text.raster_profile().antialias,
                cell_width = text.metrics().width_px.get(),
                cell_height = text.metrics().height_px.get(),
                baseline = text.metrics().baseline_px,
                line_spacing = app.config().font.line_spacing,
                atlas_format = "R8_UNORM",
                atlas_filter = "nearest",
                "resolved terminal text profile"
            );
        }
        let layout = GridLayout::calculate_with_style(
            gfx.logical_size(),
            gfx.scale(),
            content_insets(app.config()),
            text.metrics(),
            app.config().font.line_spacing,
            text.generation(),
        )?;
        let initial_size = layout.grid;
        let text_scale = gfx.scale();
        let initial_cwd = SpawnDirectory::open(&app.launch_context().base_cwd)?;
        let session = TerminalSession::start(
            app.launch(),
            initial_cwd,
            app.config(),
            initial_size,
            &app_runtime,
        )?;
        let max_count = NonZeroU8::new(app.config().tabs.max_count)
            .ok_or_else(|| UiRuntimeError::Grid("tab count cannot be zero".into()))?;
        let tabs = TabManager::bootstrap(session, app_runtime, max_count);
        let wake_backend: Arc<dyn WakeBackend> = Arc::new(wake.clone());
        let reset_font_size = app.config().font.size;
        let clipboard_workers = crate::clipboard::TransferWorkers::new(&wake);
        Ok(Self {
            state: RuntimeState::Running,
            app,
            gfx,
            wake,
            tabs,
            wake_backend,
            text,
            tab_text,
            layout,
            layout_generation: 1,
            text_scale,
            resize_settle_deadline: None,
            font_size: reset_font_size,
            reset_font_size,
            modifiers: leyline_gfx::ModifiersState::default(),
            terminal_control_gesture: false,
            keyboard_focused: false,
            cursor_blink_visible: true,
            cursor_blink_deadline: None,
            selecting: false,
            selection_point: None,
            selection_kind: None,
            selection_dragged: false,
            click_tracker: ClickTracker::default(),
            drag_scroll: None,
            link_candidate: None,
            desktop_launcher: crate::desktop::DesktopLauncher::new(),
            ime: ImeState::default(),
            ime_rectangle: None,
            clipboard_workers,
            selection: crate::selection::SelectionController::default(),
            last_input_serial: None,
            wheel_remainder_120: 0,
            scrollbar: ScrollbarController::default(),
            tab_bar: crate::tab::TabBarPresentation::default(),
        })
    }

    /// Runs the demand-driven window loop until the compositor requests close.
    ///
    /// # Errors
    /// Returns a typed platform, renderer, or application failure.
    pub fn run(mut self) -> Result<(), UiRuntimeError> {
        let result = self.run_loop();
        if let Err(error) = &result {
            tracing::error!(
                category = "runtime_error",
                error_category = ?error.category(),
                %error,
                "runtime loop failed"
            );
            self.enter_fatal_pending_exit(error.category());
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    fn run_loop(&mut self) -> Result<(), UiRuntimeError> {
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
                    PlatformEvent::Configured { .. } | PlatformEvent::ScaleChanged { .. } => {
                        self.cancel_pointer_gesture();
                        self.resize_settle_deadline = Some(Instant::now() + RESIZE_SETTLE_INTERVAL);
                        self.gfx.acknowledge_resize()?;
                    }
                    PlatformEvent::KeyboardFocus { focused, .. } => {
                        self.keyboard_focused = focused;
                        self.active_session_mut().focus_changed(focused)?;
                        if !focused {
                            self.terminal_control_gesture = false;
                            self.cancel_paste_confirmation()?;
                            self.cancel_pointer_gesture();
                            if let Some(serial) = self.gfx.disable_text_input()? {
                                self.ime.record_commit_serial(serial);
                            }
                            self.ime.deactivate();
                            self.ime_rectangle = None;
                        }
                    }
                    PlatformEvent::Key(key) => self.handle_key(&key)?,
                    PlatformEvent::ModifiersChanged(modifiers) => self.modifiers_changed(modifiers),
                    PlatformEvent::Pointer(pointer) => self.handle_pointer(pointer)?,
                    PlatformEvent::TextInput(event) => self.handle_text_input(event)?,
                    PlatformEvent::Clipboard(event) => self.handle_clipboard_event(event)?,
                    PlatformEvent::SurfaceSuspended => self.cancel_pointer_gesture(),
                    PlatformEvent::FrameReady | PlatformEvent::SurfaceResumed => {}
                }
            }
            self.apply_settled_resize()?;
            self.flush_expired_sync(Instant::now())?;
            self.drain_sessions()?;
            if self.tabs.is_empty() {
                self.finish_last_tab_shutdown()?;
                break;
            }
            self.drain_clipboard_results()?;
            self.process_drag_scroll()?;
            if self.scrollbar.expire(Instant::now()) {
                self.compose_latest()?;
            }
            if let Some(snapshot) = self.active_session_mut().end_drain_round()? {
                self.compose_snapshot(&snapshot)?;
            }
            self.advance_cursor_blink(Instant::now())?;
            self.refresh_active_title()?;
            if self.poll_shutdown()? {
                break;
            }
            let render_timeout = if self.resize_settle_deadline.is_some() {
                match self.gfx.try_render_resize_preview()? {
                    RenderOutcome::Deferred => Some(GfxRuntime::retry_delay()),
                    RenderOutcome::Rendered
                    | RenderOutcome::WaitingForFrame
                    | RenderOutcome::Idle => None,
                }
            } else {
                match self.gfx.try_render()? {
                    RenderOutcome::Deferred => Some(GfxRuntime::retry_delay()),
                    RenderOutcome::Rendered
                    | RenderOutcome::WaitingForFrame
                    | RenderOutcome::Idle => None,
                }
            };
            let shutdown_poll = self
                .tabs
                .tabs()
                .iter()
                .filter_map(|tab| tab.session.shutdown_deadline())
                .chain(self.tabs.next_closing_deadline())
                .min()
                .map(|deadline| deadline.min(Instant::now() + SHUTDOWN_POLL_INTERVAL));
            let sync_deadline = self
                .tabs
                .tabs()
                .iter()
                .filter_map(|tab| tab.session.pending_sync().map(|pending| pending.deadline))
                .min();
            let timeout = earliest_timeout(
                earliest_timeout(
                    earliest_timeout(render_timeout, self.drag_scroll.map(|drag| drag.deadline)),
                    self.resize_settle_deadline,
                ),
                shutdown_poll,
            );
            let timeout = earliest_timeout(timeout, self.scrollbar.next_deadline());
            let timeout = earliest_timeout(timeout, sync_deadline);
            let timeout = earliest_timeout(timeout, self.cursor_blink_deadline);
            if self
                .tabs
                .tabs()
                .iter()
                .all(|tab| tab.runtime.inbox_ref().prepare_to_wait())
            {
                self.gfx.poll_wait(Some(self.wake.as_fd()), timeout)?;
                self.wake.drain()?;
            }
        }
        self.app.stop()?;
        Ok(())
    }

    fn finish_last_tab_shutdown(&mut self) -> Result<(), UiRuntimeError> {
        // drain_sessions moves the final completed tab into the closing set. From this point,
        // avoid active-tab and compositor-input paths while the owned PTY workers finish.
        while !self.poll_shutdown()? {
            self.gfx
                .poll_wait(Some(self.wake.as_fd()), Some(SHUTDOWN_POLL_INTERVAL))?;
            self.wake.drain()?;
        }
        Ok(())
    }

    fn enter_fatal_pending_exit(&mut self, category: ErrorCategory) {
        if self.state == RuntimeState::FatalPendingExit {
            return;
        }
        self.state = RuntimeState::FatalPendingExit;
        tracing::error!(
            category = "fatal_pending_exit",
            error_category = ?category,
            "runtime entered fatal shutdown"
        );
        for tab in self.tabs.tabs_mut() {
            tab.runtime.fast_cancel();
        }
        let _ = self.app.request_shutdown(ShutdownReason::PlatformFailure);
        for tab in self.tabs.tabs_mut() {
            tab.session.begin_shutdown();
        }

        while let Some(deadline) = self
            .tabs
            .tabs()
            .iter()
            .filter_map(|tab| tab.session.shutdown_deadline())
            .max()
        {
            let pending = self.tabs.tabs_mut().iter_mut().any(|tab| {
                matches!(
                    tab.session.poll_shutdown(Instant::now()),
                    Ok(ShutdownPoll::Pending)
                )
            });
            if !pending {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                continue;
            }
            std::thread::sleep(remaining.min(SHUTDOWN_POLL_INTERVAL));
        }
        let _ = self.app.stop();
    }

    fn poll_shutdown(&mut self) -> Result<bool, UiRuntimeError> {
        self.tabs.poll_closing(Instant::now())?;
        if matches!(self.app.lifecycle(), crate::app::Lifecycle::ShuttingDown(_))
            && self.tabs.is_empty()
            && self.tabs.closing_is_empty()
        {
            return Ok(true);
        }
        if self
            .tabs
            .tabs()
            .iter()
            .all(|tab| tab.session.shutdown_deadline().is_none())
        {
            return Ok(false);
        }
        let mut pending = false;
        let mut timed_out = false;
        for tab in self.tabs.tabs_mut() {
            match tab.session.poll_shutdown(Instant::now())? {
                ShutdownPoll::Pending => pending = true,
                ShutdownPoll::TimedOut => timed_out = true,
                ShutdownPoll::Complete => {}
            }
        }
        if pending {
            Ok(false)
        } else {
            if timed_out {
                tracing::warn!(
                    category = "pty",
                    module = "ui_runtime",
                    "PTY shutdown exceeded the 2 second completion deadline; detached owned workers"
                );
            }
            for tab in self.tabs.tabs_mut() {
                tab.runtime.fast_cancel();
            }
            Ok(true)
        }
    }

    fn reconfigure_layout(
        &mut self,
        logical: leyline_gfx::LogicalSize,
        scale: leyline_gfx::Scale120,
        font_size: f64,
    ) -> Result<(), UiRuntimeError> {
        if scale == self.text_scale && font_size.to_bits() == self.font_size.to_bits() {
            return self.resize_layout_without_font_rebuild(logical, scale);
        }
        let request = FontRequest::from_points(
            self.app.config().font.family.clone(),
            font_size,
            scale.0,
            self.app.config().font.ligatures,
        )?
        .with_rendering(
            text_hinting(self.app.config().font.hinting),
            text_antialiasing(self.app.config().font.antialiasing),
        );
        let mut prepared = self.text.prepare_configure(request)?;
        let mut prepared_tab = self
            .tab_text
            .prepare_configure(tab_font_request(font_size, scale.0)?)?;
        let layout = GridLayout::calculate_with_style(
            logical,
            scale,
            content_insets(self.app.config()),
            prepared.metrics(),
            self.app.config().font.line_spacing,
            prepared.generation(),
        )?;
        let grid_changed = self.layout.grid != layout.grid;
        let tab_bar = crate::tab::TabBarPresentation::layout(
            &self.tabs,
            layout.viewport_px.width,
            scale.0,
            &self.app.config().tabs,
            self.tab_bar.offset,
        );
        let scene = if grid_changed {
            None
        } else if let Some(snapshot) = self.active_session().latest_snapshot().cloned() {
            let selection = self.active_session().selection_overlay(snapshot.generation);
            Some(compose(
                prepared.text_system_mut(),
                prepared_tab.text_system_mut(),
                &snapshot,
                FrameOverlays {
                    selection: &selection,
                    preedit: self.ime.preedit.as_ref(),
                    paste_confirmation: self.paste_confirmation_overlay(),
                    scrollbar: None,
                    tab_bar: Some(&tab_bar),
                },
                &layout,
                &self.app.config().colors,
                crate::frame_composer::CursorPresentationPolicy {
                    blink_phase_visible: self.cursor_blink_visible,
                },
            )?)
        } else {
            None
        };
        if grid_changed {
            for tab in self.tabs.tabs_mut() {
                if let Err(error) = tab.session.resize(layout.grid) {
                    tab.session.mark_failed();
                    tab.runtime.fast_cancel();
                    tracing::warn!(category = "tab_session_failed", session_id = tab.id.get(), %error, "tab resize failed");
                }
            }
        }
        self.text.commit_configure(prepared)?;
        self.tab_text.commit_configure(prepared_tab)?;
        self.text_scale = scale;
        self.font_size = font_size;
        self.layout = layout;
        self.layout_generation = self
            .layout_generation
            .checked_add(1)
            .ok_or_else(|| UiRuntimeError::Grid("layout generation overflow".into()))?;
        self.tab_bar = tab_bar;
        self.refresh_text_input_rectangle()?;
        if let Some(scene) = scene {
            self.gfx.apply(leyline_gfx::GfxCommand::SetScene(scene))?;
        }
        Ok(())
    }

    fn resize_layout_without_font_rebuild(
        &mut self,
        logical: leyline_gfx::LogicalSize,
        scale: leyline_gfx::Scale120,
    ) -> Result<(), UiRuntimeError> {
        let layout = GridLayout::calculate_with_style(
            logical,
            scale,
            content_insets(self.app.config()),
            self.text.metrics(),
            self.app.config().font.line_spacing,
            self.text.generation(),
        )?;
        let grid_changed = self.layout.grid != layout.grid;
        if grid_changed {
            for tab in self.tabs.tabs_mut() {
                if let Err(error) = tab.session.resize(layout.grid) {
                    tab.session.mark_failed();
                    tab.runtime.fast_cancel();
                    tracing::warn!(category = "tab_session_failed", session_id = tab.id.get(), %error, "tab resize failed");
                }
            }
        }
        self.layout = layout;
        self.layout_generation = self
            .layout_generation
            .checked_add(1)
            .ok_or_else(|| UiRuntimeError::Grid("layout generation overflow".into()))?;
        self.refresh_text_input_rectangle()?;
        if grid_changed {
            Ok(())
        } else {
            self.compose_latest()
        }
    }

    fn apply_settled_resize(&mut self) -> Result<(), UiRuntimeError> {
        let Some(deadline) = self.resize_settle_deadline else {
            return Ok(());
        };
        if Instant::now() < deadline {
            return Ok(());
        }
        self.resize_settle_deadline = None;
        self.reconfigure_layout(self.gfx.logical_size(), self.gfx.scale(), self.font_size)
    }

    fn flush_expired_sync(&mut self, now: Instant) -> Result<(), UiRuntimeError> {
        for tab in self.tabs.tabs_mut() {
            if let Some(pending) = tab.session.pending_sync()
                && now >= pending.deadline
            {
                tab.session.flush_synchronized_update(
                    pending.epoch,
                    crate::terminal::SyncFlushReason::Timeout,
                )?;
            }
        }
        Ok(())
    }

    fn advance_cursor_blink(&mut self, now: Instant) -> Result<(), UiRuntimeError> {
        let blinking = self.keyboard_focused
            && self
                .active_session()
                .latest_snapshot()
                .is_some_and(|snapshot| {
                    snapshot.cursor.visible
                        && snapshot.cursor.blink == crate::terminal::CursorBlink::Blinking
                });
        if !blinking {
            let changed = !self.cursor_blink_visible;
            self.cursor_blink_visible = true;
            self.cursor_blink_deadline = None;
            if changed {
                self.compose_latest()?;
            }
            return Ok(());
        }
        let Some(deadline) = self.cursor_blink_deadline else {
            self.cursor_blink_visible = true;
            self.cursor_blink_deadline = Some(now + Duration::from_millis(500));
            return Ok(());
        };
        if now >= deadline {
            self.cursor_blink_visible = !self.cursor_blink_visible;
            self.cursor_blink_deadline = Some(now + Duration::from_millis(500));
            self.compose_latest()?;
        }
        Ok(())
    }

    fn handle_app_event(
        &mut self,
        event: crate::app::event::AppEvent,
    ) -> Result<(), UiRuntimeError> {
        if let crate::app::event::AppEvent::Pty(pty) = &event {
            match self.active_session_mut().handle_pty_event(pty.clone())? {
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
                let cancellation = self.selection.shutdown();
                if let Some(request) = cancellation.request {
                    self.clipboard_workers.cancel(request.get());
                }
                for tab in self.tabs.tabs_mut() {
                    tab.runtime.fast_cancel();
                    tab.session.begin_shutdown();
                }
                self.cancel_pointer_gesture();
            }
            AppAction::Continue | AppAction::Stop => {}
        }
        Ok(())
    }

    fn handle_key(&mut self, key: &leyline_gfx::KeyInput) -> Result<(), UiRuntimeError> {
        if self.handle_paste_confirmation_key(key)? {
            return Ok(());
        }
        if key.state == leyline_gfx::KeyState::Released {
            return Ok(());
        }
        self.last_input_serial = Some(key.serial);
        let modifiers = terminal_modifiers(key);
        if let Some(action) = self.resolve_shortcut(key) {
            tracing::debug!(
                ?action,
                logical_key = ?key.logical_key,
                "configurable shortcut matched"
            );
            if !(key.repeat && ignores_key_repeat(action)) {
                self.execute_action(action)?;
            }
            return Ok(());
        }
        let terminal_key = match key.logical_key {
            leyline_gfx::LogicalKey::Backspace => Some(crate::terminal::TerminalKey::Backspace),
            leyline_gfx::LogicalKey::Tab => Some(crate::terminal::TerminalKey::Tab),
            leyline_gfx::LogicalKey::Enter => Some(crate::terminal::TerminalKey::Enter),
            leyline_gfx::LogicalKey::Escape => Some(crate::terminal::TerminalKey::Escape),
            leyline_gfx::LogicalKey::ArrowUp => Some(crate::terminal::TerminalKey::Up),
            leyline_gfx::LogicalKey::ArrowDown => Some(crate::terminal::TerminalKey::Down),
            leyline_gfx::LogicalKey::ArrowLeft => Some(crate::terminal::TerminalKey::Left),
            leyline_gfx::LogicalKey::ArrowRight => Some(crate::terminal::TerminalKey::Right),
            leyline_gfx::LogicalKey::Home => Some(crate::terminal::TerminalKey::Home),
            leyline_gfx::LogicalKey::End => Some(crate::terminal::TerminalKey::End),
            leyline_gfx::LogicalKey::Insert => Some(crate::terminal::TerminalKey::Insert),
            leyline_gfx::LogicalKey::Delete => Some(crate::terminal::TerminalKey::Delete),
            leyline_gfx::LogicalKey::PageUp => Some(crate::terminal::TerminalKey::PageUp),
            leyline_gfx::LogicalKey::PageDown => Some(crate::terminal::TerminalKey::PageDown),
            leyline_gfx::LogicalKey::Function(number) => {
                Some(crate::terminal::TerminalKey::Function(number))
            }
            _ => None,
        };
        if let Some(key) = terminal_key {
            self.active_session_mut().input_key(key, modifiers)?;
        } else if let Some(text) = key_text(key) {
            if modifiers.control || modifiers.alt {
                if let Some(ch) = match key.logical_key {
                    leyline_gfx::LogicalKey::Character(ch) => Some(ch),
                    _ => text.chars().next(),
                } {
                    self.active_session_mut()
                        .input_key(crate::terminal::TerminalKey::Char(ch), modifiers)?;
                    if modifiers.control {
                        self.terminal_control_gesture = true;
                    }
                }
            } else {
                // Wayland still delivers unconsumed printable keys while text-input is enabled.
                self.active_session_mut().commit_text(&text)?;
            }
        }
        Ok(())
    }

    fn modifiers_changed(&mut self, modifiers: leyline_gfx::ModifiersState) {
        self.modifiers = modifiers;
        if !modifiers.control {
            self.terminal_control_gesture = false;
        }
    }

    fn handle_text_input(
        &mut self,
        event: leyline_gfx::TextInputEvent,
    ) -> Result<(), UiRuntimeError> {
        use leyline_gfx::TextInputEvent;
        if matches!(
            self.selection.interaction_mode(),
            crate::selection::InteractionMode::ConfirmPaste { .. }
        ) {
            return Ok(());
        }
        if !self.ime.is_active() && !matches!(&event, TextInputEvent::Enter | TextInputEvent::Leave)
        {
            // Compositors may leave already-queued text-input events behind a focus/leave event.
            tracing::debug!(
                category = "stale_text_input",
                "ignored event for inactive IME"
            );
            return Ok(());
        }
        match event {
            TextInputEvent::Enter => {
                self.ime.activate();
                self.ime_rectangle = None;
                self.enable_text_input()?;
            }
            TextInputEvent::Leave => {
                self.ime.deactivate();
                self.ime_rectangle = None;
            }
            TextInputEvent::Preedit { text, cursor } => {
                self.ime.preedit_string(text, cursor)?;
            }
            TextInputEvent::Commit(text) => self.ime.commit_string(text)?,
            TextInputEvent::DeleteSurrounding {
                before_bytes,
                after_bytes,
            } => self
                .ime
                .delete_surrounding_text(before_bytes, after_bytes)?,
            TextInputEvent::Done { serial } => {
                let Some(snapshot) = self.active_session().latest_snapshot().cloned() else {
                    return Ok(());
                };
                let anchor = [snapshot.cursor.column, snapshot.cursor.line];
                let done = self.ime.done(serial, snapshot.generation, anchor)?;
                if let Some(commit) = done.commit {
                    let text = std::str::from_utf8(&commit)
                        .map_err(|_| crate::interaction::ImeError::CommitTooLarge)?;
                    self.active_session_mut().commit_text(text)?;
                }
                if done.delete_ignored {
                    tracing::warn!("IME delete-surrounding request ignored for terminal input");
                }
                if done.outbound_resend_required {
                    self.refresh_text_input_rectangle()?;
                }
                self.compose_latest()?;
            }
        }
        Ok(())
    }

    fn refresh_text_input_rectangle(&mut self) -> Result<(), UiRuntimeError> {
        if !self.ime.is_active() || !self.gfx.text_input_available() {
            return Ok(());
        }
        let Some(rectangle) = self.text_input_rectangle() else {
            return Ok(());
        };
        if self.ime_rectangle == Some(rectangle) && !self.ime.outbound.dirty {
            return Ok(());
        }
        let serial = self.gfx.update_text_input(rectangle)?;
        if let Some(serial) = serial {
            self.ime.record_commit_serial(serial);
            self.ime_rectangle = Some(rectangle);
        }
        Ok(())
    }

    fn enable_text_input(&mut self) -> Result<(), UiRuntimeError> {
        if !self.ime.is_active() || !self.gfx.text_input_available() {
            return Ok(());
        }
        let Some(rectangle) = self.text_input_rectangle() else {
            return Ok(());
        };
        if let Some(serial) = self.gfx.enable_text_input(rectangle)? {
            self.ime.record_commit_serial(serial);
            self.ime_rectangle = Some(rectangle);
        }
        Ok(())
    }

    fn text_input_rectangle(&self) -> Option<leyline_gfx::TextInputRectangle> {
        let snapshot = self.active_session().latest_snapshot()?;
        let scale = self.gfx.scale().0.max(1);
        let physical_x = self.layout.content_origin_px[0].saturating_add(
            u32::from(snapshot.cursor.column) * u32::from(self.layout.cell_px[0].get()),
        );
        let physical_y = self.layout.content_origin_px[1].saturating_add(
            u32::from(snapshot.cursor.line) * u32::from(self.layout.cell_px[1].get()),
        );
        let logical = |value: u32| {
            i32::try_from(u64::from(value) * 120 / u64::from(scale)).unwrap_or(i32::MAX)
        };
        Some(leyline_gfx::TextInputRectangle {
            x: logical(physical_x),
            y: logical(physical_y),
            width: logical(u32::from(self.layout.cell_px[0].get())).max(1),
            height: logical(u32::from(self.layout.cell_px[1].get())).max(1),
        })
    }

    fn compose_latest(&mut self) -> Result<(), UiRuntimeError> {
        let Some(snapshot) = self.active_session().latest_snapshot().cloned() else {
            return Ok(());
        };
        self.compose_snapshot(&snapshot)
    }

    fn compose_snapshot(
        &mut self,
        snapshot: &crate::terminal::FrameSnapshot,
    ) -> Result<(), UiRuntimeError> {
        self.update_tab_bar();
        self.ime.reanchor_preedit(
            snapshot.generation,
            [snapshot.cursor.column, snapshot.cursor.line],
        );
        self.refresh_text_input_rectangle()?;
        let paste_confirmation = self.paste_confirmation_overlay().copied();
        let selection = self.active_session().selection_overlay(snapshot.generation);
        let geometry = ScrollbarGeometry::calculate(
            snapshot,
            &self.layout,
            &self.app.config().scrollbar,
            self.gfx.scale().0,
        );
        let scrollbar = geometry.and_then(|geometry| {
            self.scrollbar.presentation(
                geometry,
                snapshot,
                &self.app.config().scrollbar,
                Instant::now(),
            )
        });
        let scene = compose(
            &mut self.text,
            &mut self.tab_text,
            snapshot,
            FrameOverlays {
                selection: &selection,
                preedit: self.ime.preedit.as_ref(),
                paste_confirmation: paste_confirmation.as_ref(),
                scrollbar: scrollbar.as_ref(),
                tab_bar: Some(&self.tab_bar),
            },
            &self.layout,
            &self.app.config().colors,
            crate::frame_composer::CursorPresentationPolicy {
                blink_phase_visible: self.cursor_blink_visible,
            },
        )?;
        self.gfx.apply(leyline_gfx::GfxCommand::SetScene(scene))?;
        Ok(())
    }

    fn update_tab_bar(&mut self) {
        self.tab_bar = crate::tab::TabBarPresentation::layout(
            &self.tabs,
            self.layout.viewport_px.width,
            self.gfx.scale().0,
            &self.app.config().tabs,
            self.tab_bar.offset,
        );
    }

    fn paste_confirmation_overlay(&self) -> Option<&crate::clipboard::PasteConfirmationOverlay> {
        self.selection.overlay()
    }

    fn enter_paste_confirmation(&mut self) -> Result<(), UiRuntimeError> {
        let suspended = self.ime.is_active();
        self.selection.set_modal_ime_suspended(suspended);
        if suspended {
            let serial = match self.gfx.disable_text_input() {
                Ok(serial) => serial,
                Err(error) => {
                    let _ = self.selection.cancel_paste();
                    return Err(error.into());
                }
            };
            if let Some(serial) = serial {
                self.ime.record_commit_serial(serial);
            }
            self.ime.deactivate();
            self.ime_rectangle = None;
        }
        if let Some(overlay) = self.selection.overlay() {
            tracing::warn!(
                bytes = overlay.bytes,
                lines = overlay.lines,
                risk = ?overlay.risk,
                "paste requires keyboard confirmation"
            );
        }
        self.compose_latest()
    }

    fn cancel_paste_confirmation(&mut self) -> Result<(), UiRuntimeError> {
        let cancellation = self.selection.cancel_paste();
        if let Some(request) = cancellation.request {
            self.clipboard_workers.cancel(request.get());
        }
        self.restore_ime_after_paste_confirmation(cancellation.resume_ime)?;
        self.compose_latest()
    }

    fn restore_ime_after_paste_confirmation(
        &mut self,
        suspended: bool,
    ) -> Result<(), UiRuntimeError> {
        if suspended {
            self.ime.activate();
            self.enable_text_input()?;
        }
        Ok(())
    }

    fn handle_paste_confirmation_key(
        &mut self,
        key: &leyline_gfx::KeyInput,
    ) -> Result<bool, UiRuntimeError> {
        let resume_ime = self.selection.modal_ime_suspended();
        let active = self
            .tabs
            .active_id()
            .expect("runtime always has an active tab");
        let outcome = self.selection.confirmation_input(key, active);
        if outcome == crate::selection::ConfirmationOutcome::NotActive {
            return Ok(false);
        }
        tracing::debug!(?outcome, logical_key = ?key.logical_key, "paste confirmation key handled");
        match outcome {
            crate::selection::ConfirmationOutcome::Paste { owner, text } => {
                self.restore_ime_after_paste_confirmation(resume_ime)?;
                self.compose_latest()?;
                self.paste_for_owner(owner, &text)?;
            }
            crate::selection::ConfirmationOutcome::Closed => {
                self.restore_ime_after_paste_confirmation(resume_ime)?;
                self.compose_latest()?;
            }
            crate::selection::ConfirmationOutcome::Consumed => {}
            crate::selection::ConfirmationOutcome::NotActive => return Ok(false),
        }
        Ok(true)
    }

    fn resolve_shortcut(&self, key: &leyline_gfx::KeyInput) -> Option<crate::config::Action> {
        match crate::input::shortcut::resolve_with_terminal_gesture(
            &self.app.config().keybindings,
            key,
            self.terminal_control_gesture,
        ) {
            crate::input::shortcut::ShortcutResult::Matched(action) => Some(action),
            crate::input::shortcut::ShortcutResult::NotMatched => None,
        }
    }

    fn execute_action(&mut self, action: crate::config::Action) -> Result<(), UiRuntimeError> {
        use crate::config::Action;
        match action {
            Action::CopyClipboard => {
                self.copy_selection(leyline_gfx::SelectionTarget::Clipboard)?;
            }
            Action::PasteClipboard => {
                self.request_paste(leyline_gfx::SelectionTarget::Clipboard)?;
            }
            Action::IncreaseFontSize => {
                let font_size = (self.font_size + 1.0).min(72.0);
                self.reconfigure_layout(self.gfx.logical_size(), self.gfx.scale(), font_size)?;
            }
            Action::DecreaseFontSize => {
                let font_size = (self.font_size - 1.0).max(6.0);
                self.reconfigure_layout(self.gfx.logical_size(), self.gfx.scale(), font_size)?;
            }
            Action::ResetFontSize => {
                self.reconfigure_layout(
                    self.gfx.logical_size(),
                    self.gfx.scale(),
                    self.reset_font_size,
                )?;
            }
            Action::ScrollPageUp => {
                let lines =
                    i32::try_from(self.layout.grid.lines().saturating_sub(1)).unwrap_or(i32::MAX);
                self.active_session_mut().scroll(lines)?;
                self.scrollbar.note_scroll(Instant::now());
            }
            Action::ScrollPageDown => {
                let lines =
                    -i32::try_from(self.layout.grid.lines().saturating_sub(1)).unwrap_or(i32::MAX);
                self.active_session_mut().scroll(lines)?;
                self.scrollbar.note_scroll(Instant::now());
            }
            Action::PastePrimary => {
                self.request_paste(leyline_gfx::SelectionTarget::Primary)?;
            }
            Action::NewTab => self.new_tab()?,
            Action::CloseTab => self.close_active_tab(ShutdownReason::UserRequested)?,
            Action::PreviousTab => self.switch_relative(-1)?,
            Action::NextTab => self.switch_relative(1)?,
            Action::ActivateTab(ordinal) => self.switch_ordinal(ordinal)?,
        }
        Ok(())
    }

    fn new_tab(&mut self) -> Result<(), UiRuntimeError> {
        if !matches!(self.app.lifecycle(), crate::app::Lifecycle::Running) {
            return Ok(());
        }
        if !self.tabs.has_capacity() {
            tracing::warn!(
                category = "tab_create_failed",
                limit = self.app.config().tabs.max_count,
                "tab limit reached"
            );
            return Ok(());
        }
        let source_id = self.tabs.active_id();
        let (primary, origin) = select_new_tab_cwd(
            &self.app.config().tabs.new_tab_cwd,
            self.tabs.active(),
            self.app.launch_context(),
        );
        let base = &self.app.launch_context().base_cwd;
        let (cwd, final_origin) = match SpawnDirectory::open(&primary) {
            Ok(cwd) => (cwd, origin),
            Err(error) if primary != *base => {
                tracing::warn!(
                    category = "tab_cwd",
                    session_id = source_id.map(crate::tab::SessionId::get),
                    policy = cwd_policy_name(&self.app.config().tabs.new_tab_cwd),
                    origin,
                    result = "fallback",
                    errno = error.raw_os_error(),
                    "new tab cwd candidate unavailable"
                );
                match SpawnDirectory::open(base) {
                    Ok(cwd) => (cwd, "base"),
                    Err(base_error) => {
                        tracing::warn!(
                            category = "tab_create_failed",
                            reason = "base_cwd_unavailable",
                            errno = base_error.raw_os_error(),
                            "could not create tab"
                        );
                        return Ok(());
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    category = "tab_create_failed",
                    reason = "base_cwd_unavailable",
                    errno = error.raw_os_error(),
                    "could not create tab"
                );
                return Ok(());
            }
        };
        let runtime = AppRuntimeBuilder::new(self.wake_backend.clone()).build()?;
        let session = match TerminalSession::start(
            self.app.launch(),
            cwd,
            self.app.config(),
            self.layout.grid,
            &runtime,
        ) {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!(category = "tab_create_failed", %error, "could not create tab");
                return Ok(());
            }
        };
        self.quiesce_active_interaction()?;
        match self
            .tabs
            .push(session, runtime, launch_title(self.app.launch()))
        {
            Ok(id) => tracing::info!(
                category = "tab_created",
                session_id = id.get(),
                cwd_origin = final_origin,
                "tab created"
            ),
            Err(error) => {
                tracing::warn!(category = "tab_create_failed", %error, "could not create tab");
            }
        }
        self.restore_active_interaction()?;
        let snapshot = self.active_session_mut().end_drain_round()?;
        if let Some(snapshot) = snapshot {
            self.compose_snapshot(&snapshot)?;
        }
        self.refresh_active_title()
    }

    fn close_active_tab(&mut self, reason: ShutdownReason) -> Result<(), UiRuntimeError> {
        self.quiesce_active_interaction()?;
        self.active_session_mut().finish_io_round()?;
        let closed = self.tabs.close_active();
        if let Some(id) = closed {
            tracing::info!(
                category = "tab_close_requested",
                session_id = id.get(),
                "tab closing"
            );
        }
        if self.tabs.is_empty() {
            self.handle_app_event(crate::app::event::AppEvent::ShutdownRequested(reason))?;
            return Ok(());
        }
        self.restore_active_interaction()?;
        if let Some(snapshot) = self.active_session_mut().end_drain_round()? {
            self.compose_snapshot(&snapshot)?;
        }
        self.refresh_active_title()
    }

    fn switch_relative(&mut self, delta: i8) -> Result<(), UiRuntimeError> {
        self.quiesce_active_interaction()?;
        if matches!(
            self.tabs.activate_relative(delta),
            crate::tab::Activation::Changed { .. }
        ) {
            if let Some(snapshot) = self.active_session_mut().end_drain_round()? {
                self.compose_snapshot(&snapshot)?;
            }
            self.refresh_active_title()?;
        }
        self.restore_active_interaction()?;
        Ok(())
    }

    fn switch_ordinal(&mut self, ordinal: u8) -> Result<(), UiRuntimeError> {
        self.quiesce_active_interaction()?;
        if matches!(
            self.tabs.activate_ordinal(ordinal),
            crate::tab::Activation::Changed { .. }
        ) {
            if let Some(snapshot) = self.active_session_mut().end_drain_round()? {
                self.compose_snapshot(&snapshot)?;
            }
            self.refresh_active_title()?;
        }
        self.restore_active_interaction()?;
        Ok(())
    }

    fn switch_to(&mut self, id: crate::tab::SessionId) -> Result<(), UiRuntimeError> {
        self.quiesce_active_interaction()?;
        if matches!(
            self.tabs.activate(id),
            Ok(crate::tab::Activation::Changed { .. })
        ) {
            if let Some(snapshot) = self.active_session_mut().snapshot_if_dirty()? {
                self.compose_snapshot(&snapshot)?;
            } else {
                self.compose_latest()?;
            }
            self.refresh_active_title()?;
        }
        self.restore_active_interaction()?;
        Ok(())
    }

    fn quiesce_active_interaction(&mut self) -> Result<(), UiRuntimeError> {
        self.cancel_paste_confirmation()?;
        self.cancel_pointer_gesture();
        if let Some(serial) = self.gfx.disable_text_input()? {
            self.ime.record_commit_serial(serial);
        }
        self.ime.deactivate();
        self.ime_rectangle = None;
        if self.keyboard_focused {
            self.active_session_mut().focus_changed(false)?;
        }
        Ok(())
    }

    fn restore_active_interaction(&mut self) -> Result<(), UiRuntimeError> {
        if self.keyboard_focused {
            self.active_session_mut().focus_changed(true)?;
            self.ime.activate();
            self.enable_text_input()?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn handle_pointer(&mut self, event: leyline_gfx::PointerInput) -> Result<(), UiRuntimeError> {
        if matches!(
            self.selection.interaction_mode(),
            crate::selection::InteractionMode::ConfirmPaste { .. }
        ) {
            return Ok(());
        }
        if let leyline_gfx::PointerKind::Release { serial, .. } = event.kind {
            self.last_input_serial = Some(serial);
        }
        let scale = f64::from(self.gfx.scale().0) / 120.0;
        if !event.position.0.is_finite() || !event.position.1.is_finite() {
            return Ok(());
        }
        if event.position.0 < 0.0
            && !matches!(
                self.scrollbar.interaction(),
                crate::interaction::ScrollbarInteraction::Dragging { .. }
            )
        {
            return Ok(());
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pixel = [
            (event.position.0 * scale).floor() as u32,
            (event.position.1 * scale).max(0.0).floor() as u32,
        ];
        if self.tab_bar.bar.is_some_and(|bar| bar.contains(pixel)) {
            match event.kind {
                leyline_gfx::PointerKind::Press { button: 0x110, .. } => {
                    if let Some((id, close)) = self.tab_bar.hit(pixel) {
                        self.switch_to(id)?;
                        if close {
                            self.close_active_tab(ShutdownReason::UserRequested)?;
                        }
                    }
                }
                leyline_gfx::PointerKind::Press { button: 0x112, .. } => {
                    if let Some((id, _)) = self.tab_bar.hit(pixel) {
                        self.switch_to(id)?;
                        self.close_active_tab(ShutdownReason::UserRequested)?;
                    }
                }
                leyline_gfx::PointerKind::Axis {
                    horizontal_120,
                    vertical_120,
                    ..
                } => {
                    let delta = if horizontal_120 != 0 {
                        horizontal_120
                    } else {
                        vertical_120
                    };
                    let step = u32::from(self.app.config().tabs.min_width)
                        .saturating_mul(self.gfx.scale().0)
                        / 120;
                    self.tab_bar.offset = if delta > 0 {
                        self.tab_bar
                            .offset
                            .saturating_add(step)
                            .min(self.tab_bar.max_offset)
                    } else {
                        self.tab_bar.offset.saturating_sub(step)
                    };
                    self.compose_latest()?;
                }
                _ => {}
            }
            return Ok(());
        }
        let above_grid = event.position.1 * scale < f64::from(self.layout.content_origin_px[1]);
        let point = self
            .layout
            .cell_at_pixel(pixel)
            .map(|[column, line]| crate::terminal::SelectionPoint { column, line });
        if self.handle_scrollbar_pointer(&event, [f64::from(pixel[0]), f64::from(pixel[1])])? {
            return Ok(());
        }
        let modifiers = crate::terminal::Modifiers {
            shift: self.modifiers.shift,
            control: self.modifiers.control,
            alt: self.modifiers.alt,
            super_key: self.modifiers.super_key,
        };
        match event.kind {
            leyline_gfx::PointerKind::Press {
                button: 0x110,
                time_ms,
                ..
            } if point.is_some() => {
                let point = point.expect("guarded pointer cell");
                if modifiers.control && !modifiers.shift && !modifiers.alt && !modifiers.super_key {
                    self.link_candidate = self.active_session().hyperlink_at(point).map(
                        |(snapshot_generation, hyperlink, _)| LinkCandidate {
                            snapshot_generation,
                            hyperlink,
                            point,
                            modifiers: self.modifiers,
                        },
                    );
                    if self.link_candidate.is_some() {
                        return Ok(());
                    }
                }
                if !self.active_session_mut().pointer_report(
                    crate::terminal::MouseButton::Left,
                    crate::terminal::ButtonState::Pressed,
                    point,
                    modifiers,
                )? {
                    let kind = self.click_tracker.register(0x110, point, time_ms);
                    self.active_session_mut()
                        .start_selection_kind(kind, point)?;
                    self.selecting = true;
                    self.selection_point = Some(point);
                    self.selection_kind = Some(kind);
                    self.selection_dragged = false;
                }
            }
            leyline_gfx::PointerKind::Release { button: 0x110, .. } => {
                if let Some(candidate) = self.link_candidate.take() {
                    if let Some(point) = point
                        && let Some((generation, hyperlink, uri)) =
                            self.active_session().hyperlink_at(point)
                        && candidate.matches(generation, hyperlink, point, self.modifiers)
                        && let Err(error) = self.desktop_launcher.open(&uri)
                    {
                        tracing::warn!(%error, "link open request rejected");
                    }
                    return Ok(());
                }
                let point = point.or(self.selection_point);
                let Some(point) = point else {
                    self.cancel_pointer_gesture();
                    return Ok(());
                };
                self.selection_dragged |= self.selection_point != Some(point);
                if !self.active_session_mut().pointer_report(
                    crate::terminal::MouseButton::Left,
                    crate::terminal::ButtonState::Released,
                    point,
                    modifiers,
                )? && self.selecting
                {
                    self.active_session_mut().update_selection(point)?;
                }
                if self.selecting
                    && keep_selection_after_release(self.selection_kind, self.selection_dragged)
                {
                    self.copy_selection(leyline_gfx::SelectionTarget::Primary)?;
                } else if self.selecting {
                    self.active_session_mut().clear_selection()?;
                }
                self.selecting = false;
                self.selection_point = None;
                self.selection_kind = None;
                self.selection_dragged = false;
                self.drag_scroll = None;
            }
            leyline_gfx::PointerKind::Press { button: 0x112, .. } if point.is_some() => {
                self.request_paste(leyline_gfx::SelectionTarget::Primary)?;
            }
            leyline_gfx::PointerKind::Motion { .. } if self.selecting => {
                if let Some(point) = point {
                    self.selection_dragged |= self.selection_point != Some(point);
                    self.active_session_mut().update_selection(point)?;
                    self.selection_point = Some(point);
                    self.drag_scroll = None;
                } else if let Some((direction, point)) = self.drag_scroll_target(pixel, above_grid)
                {
                    self.selection_dragged = true;
                    self.selection_point = Some(point);
                    self.drag_scroll = Some(DragScroll {
                        direction,
                        point,
                        deadline: Instant::now() + DRAG_SCROLL_INTERVAL,
                    });
                } else {
                    self.drag_scroll = None;
                }
            }
            leyline_gfx::PointerKind::Axis { vertical_120, .. }
                if vertical_120 != 0 && point.is_some() =>
            {
                let point = point.expect("guarded pointer cell");
                let steps = accumulate_wheel_steps(&mut self.wheel_remainder_120, vertical_120);
                if steps == 0 {
                    return Ok(());
                }
                let button = if steps < 0 {
                    crate::terminal::MouseButton::WheelUp
                } else {
                    crate::terminal::MouseButton::WheelDown
                };
                let mut reported = false;
                for _ in 0..steps.unsigned_abs() {
                    reported |= self.active_session_mut().pointer_report(
                        button,
                        crate::terminal::ButtonState::Pressed,
                        point,
                        modifiers,
                    )?;
                }
                if !reported {
                    let lines = -steps * 3;
                    if modifiers.shift || !self.active_session_mut().alternate_scroll(lines)? {
                        self.active_session_mut().scroll(lines)?;
                        self.scrollbar.note_scroll(Instant::now());
                    }
                }
            }
            leyline_gfx::PointerKind::Leave { .. } => self.cancel_pointer_gesture(),
            _ => {}
        }
        Ok(())
    }

    fn handle_scrollbar_pointer(
        &mut self,
        event: &leyline_gfx::PointerInput,
        point: [f64; 2],
    ) -> Result<bool, UiRuntimeError> {
        let Some(snapshot) = self.active_session().latest_snapshot().cloned() else {
            return Ok(false);
        };
        let Some(geometry) = ScrollbarGeometry::calculate(
            &snapshot,
            &self.layout,
            &self.app.config().scrollbar,
            self.gfx.scale().0,
        ) else {
            return Ok(false);
        };
        let now = Instant::now();
        let previous = self.scrollbar.interaction();
        match event.kind {
            leyline_gfx::PointerKind::Press { button: 0x110, .. }
                if geometry.hit.contains(point) =>
            {
                self.selecting = false;
                self.selection_point = None;
                self.selection_kind = None;
                self.selection_dragged = false;
                self.drag_scroll = None;
                self.link_candidate = None;
                self.click_tracker.reset();
                if let Some(offset) = self.scrollbar.press(
                    point,
                    geometry,
                    snapshot.grid.lines(),
                    snapshot.display_offset,
                    now,
                ) {
                    self.active_session_mut().scroll_to_display_offset(offset)?;
                }
                self.compose_latest()?;
                return Ok(true);
            }
            leyline_gfx::PointerKind::Motion { .. } | leyline_gfx::PointerKind::Enter { .. } => {
                if let Some(offset) = self.scrollbar.pointer_motion(point, geometry, now) {
                    self.active_session_mut().scroll_to_display_offset(offset)?;
                }
                if previous != self.scrollbar.interaction() {
                    self.compose_latest()?;
                }
                if matches!(
                    self.scrollbar.interaction(),
                    crate::interaction::ScrollbarInteraction::Dragging { .. }
                ) || geometry.hit.contains(point)
                {
                    return Ok(true);
                }
            }
            leyline_gfx::PointerKind::Release { button: 0x110, .. }
                if matches!(
                    previous,
                    crate::interaction::ScrollbarInteraction::Dragging { .. }
                ) =>
            {
                self.scrollbar.release();
                self.compose_latest()?;
                return Ok(true);
            }
            leyline_gfx::PointerKind::Axis { vertical_120, .. }
                if geometry.hit.contains(point) && vertical_120 != 0 =>
            {
                let steps = accumulate_wheel_steps(&mut self.wheel_remainder_120, vertical_120);
                if steps != 0 {
                    self.active_session_mut().scroll(-steps * 3)?;
                    self.scrollbar.note_scroll(now);
                }
                return Ok(true);
            }
            leyline_gfx::PointerKind::Leave { .. }
                if matches!(
                    previous,
                    crate::interaction::ScrollbarInteraction::Dragging { .. }
                ) =>
            {
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn drag_scroll_target(
        &self,
        pixel: [u32; 2],
        above_grid: bool,
    ) -> Option<(i32, crate::terminal::SelectionPoint)> {
        let origin = self.layout.content_origin_px;
        let width = u32::from(self.layout.grid.columns.get())
            .checked_mul(u32::from(self.layout.cell_px[0].get()))?;
        let height = u32::from(self.layout.grid.lines.get())
            .checked_mul(u32::from(self.layout.cell_px[1].get()))?;
        if pixel[0] < origin[0] || pixel[0] >= origin[0].checked_add(width)? {
            return None;
        }
        let column =
            u16::try_from((pixel[0] - origin[0]) / u32::from(self.layout.cell_px[0].get())).ok()?;
        if above_grid {
            Some((1, crate::terminal::SelectionPoint { column, line: 0 }))
        } else if pixel[1] >= origin[1].checked_add(height)? {
            Some((
                -1,
                crate::terminal::SelectionPoint {
                    column,
                    line: self.layout.grid.lines.get() - 1,
                },
            ))
        } else {
            None
        }
    }

    fn process_drag_scroll(&mut self) -> Result<(), UiRuntimeError> {
        let Some(mut drag) = self.drag_scroll else {
            return Ok(());
        };
        let now = Instant::now();
        if now < drag.deadline {
            return Ok(());
        }
        self.active_session_mut().scroll(drag.direction)?;
        self.active_session_mut().update_selection(drag.point)?;
        drag.deadline = now + DRAG_SCROLL_INTERVAL;
        self.drag_scroll = Some(drag);
        Ok(())
    }

    fn cancel_pointer_gesture(&mut self) {
        self.selecting = false;
        self.selection_point = None;
        self.selection_kind = None;
        self.selection_dragged = false;
        self.drag_scroll = None;
        self.link_candidate = None;
        self.click_tracker.reset();
        self.scrollbar.cancel();
    }

    fn copy_selection(
        &mut self,
        target: leyline_gfx::SelectionTarget,
    ) -> Result<(), UiRuntimeError> {
        let (Some(serial), Some(text)) = (
            self.last_input_serial,
            self.active_session().selected_text(),
        ) else {
            tracing::debug!(
                ?target,
                "copy ignored because no input serial or selection is available"
            );
            return Ok(());
        };
        let bytes = text.len();
        if bytes == 0 {
            tracing::debug!(?target, "copy ignored because the selection is empty");
            return Ok(());
        }
        let transfer_target = selection_transfer_target(target);
        let Some(start) = self.selection.prepare_publish(transfer_target, text) else {
            tracing::warn!(?target, bytes, "copy selection rejected by size policy");
            return Ok(());
        };
        let published = match self
            .gfx
            .publish_selection(target, start.source.get(), serial)
        {
            Ok(published) => published,
            Err(error) => {
                self.selection.publish_failed(start.source);
                return Err(error.into());
            }
        };
        if published {
            self.selection
                .publish_submitted(transfer_target, start.source);
            tracing::debug!(
                ?target,
                source = start.source.get(),
                bytes,
                "selection source published"
            );
        } else {
            self.selection.publish_failed(start.source);
            tracing::warn!(?target, "selection protocol is unavailable");
        }
        Ok(())
    }

    fn request_paste(
        &mut self,
        target: leyline_gfx::SelectionTarget,
    ) -> Result<(), UiRuntimeError> {
        let transfer_target = selection_transfer_target(target);
        let owner = self
            .tabs
            .active_id()
            .expect("runtime always has an active tab");
        let Some(start) = self.selection.begin_request(transfer_target, owner) else {
            return Ok(());
        };
        if let Some(request) = start.cancel.request {
            self.clipboard_workers.cancel(request.get());
        }
        self.restore_ime_after_paste_confirmation(start.cancel.resume_ime)?;
        if start.cancel.overlay_changed {
            self.compose_latest()?;
        }
        let fd = match self.gfx.receive_selection(target) {
            Ok(Some(fd)) => fd,
            Ok(None) => {
                self.selection.request_failed(start.request);
                tracing::warn!(
                    ?target,
                    "paste ignored because no compatible selection offer is available"
                );
                return Ok(());
            }
            Err(error) => {
                self.selection.request_failed(start.request);
                return Err(error.into());
            }
        };
        tracing::debug!(
            ?target,
            request = start.request.get(),
            "selection receive requested"
        );
        if let Err(error) = self
            .clipboard_workers
            .receive(start.request.get(), transfer_target, fd)
        {
            self.selection.request_failed(start.request);
            tracing::warn!(%error, "clipboard receive queue rejected transfer");
        }
        Ok(())
    }

    fn handle_clipboard_event(
        &mut self,
        event: leyline_gfx::ClipboardEvent,
    ) -> Result<(), UiRuntimeError> {
        match event {
            leyline_gfx::ClipboardEvent::Send {
                target,
                source,
                mime_type,
                fd,
            } => {
                let supported = matches!(
                    mime_type.as_str(),
                    "text/plain;charset=utf-8" | "UTF8_STRING" | "text/plain"
                );
                let transfer_target = selection_transfer_target(target);
                if supported
                    && let Some(bytes) = self.selection.source_bytes(
                        transfer_target,
                        crate::selection::SourceToken::from_raw(source),
                    )
                    && let Err(error) =
                        self.clipboard_workers
                            .send(transfer_target, source, fd, bytes)
                {
                    tracing::warn!(%error, ?target, source, "clipboard send queue rejected transfer");
                }
            }
            leyline_gfx::ClipboardEvent::SourceCancelled { target, source } => {
                self.selection.source_cancelled(
                    selection_transfer_target(target),
                    crate::selection::SourceToken::from_raw(source),
                );
            }
            leyline_gfx::ClipboardEvent::Offer { target, mime_types } => {
                let state = if mime_types.iter().any(|mime| {
                    matches!(
                        mime.as_str(),
                        "text/plain;charset=utf-8" | "UTF8_STRING" | "text/plain"
                    )
                }) {
                    crate::selection::OfferState::TextAvailable
                } else {
                    crate::selection::OfferState::Unsupported
                };
                let cancellation = self
                    .selection
                    .offer_changed(selection_transfer_target(target), state);
                self.apply_paste_cancellation(cancellation)?;
            }
            leyline_gfx::ClipboardEvent::Cleared(target) => {
                let cancellation = self.selection.offer_changed(
                    selection_transfer_target(target),
                    crate::selection::OfferState::Empty,
                );
                self.apply_paste_cancellation(cancellation)?;
            }
            leyline_gfx::ClipboardEvent::Unavailable(target) => {
                let cancellation = self.selection.offer_changed(
                    selection_transfer_target(target),
                    crate::selection::OfferState::Unavailable,
                );
                self.apply_paste_cancellation(cancellation)?;
            }
        }
        Ok(())
    }

    fn drain_clipboard_results(&mut self) -> Result<(), UiRuntimeError> {
        let mut results = Vec::new();
        let retained = self
            .clipboard_workers
            .drain_round(CLIPBOARD_RESULT_BUDGET, |result| results.push(result));
        for result in results {
            match result {
                crate::clipboard::TransferResult::Received {
                    request,
                    target,
                    result,
                } => {
                    self.clipboard_workers.finish_request(request);
                    let active = self
                        .tabs
                        .active_id()
                        .expect("runtime always has an active tab");
                    match self.selection.transfer_completed(
                        crate::selection::RequestToken::from_raw(request),
                        target,
                        result,
                        self.app.config().behavior.confirm_multiline_paste,
                        active,
                    ) {
                        crate::selection::PasteTransition::Paste { owner, text } => {
                            self.paste_for_owner(owner, &text)?;
                        }
                        crate::selection::PasteTransition::Confirming => {
                            self.enter_paste_confirmation()?;
                        }
                        crate::selection::PasteTransition::Rejected => {
                            tracing::warn!("clipboard paste rejected by policy");
                        }
                        crate::selection::PasteTransition::Failed(error) => {
                            tracing::warn!(%error, "clipboard transfer failed");
                        }
                        crate::selection::PasteTransition::Noop
                        | crate::selection::PasteTransition::IgnoreStale => {}
                    }
                }
                crate::clipboard::TransferResult::WriteFailed {
                    target,
                    source,
                    error,
                } => {
                    tracing::warn!(%error, ?target, source, "clipboard transfer failed");
                }
            }
        }
        if retained {
            self.wake.signal()?;
        }
        Ok(())
    }

    fn apply_paste_cancellation(
        &mut self,
        cancellation: crate::selection::PasteCancellation,
    ) -> Result<(), UiRuntimeError> {
        if let Some(request) = cancellation.request {
            self.clipboard_workers.cancel(request.get());
        }
        self.restore_ime_after_paste_confirmation(cancellation.resume_ime)?;
        if cancellation.overlay_changed {
            self.compose_latest()?;
        }
        Ok(())
    }

    fn paste_for_owner(
        &mut self,
        owner: crate::tab::SessionId,
        text: &str,
    ) -> Result<(), UiRuntimeError> {
        if self.tabs.active_id() != Some(owner) {
            tracing::debug!(
                session_id = owner.get(),
                "stale paste owner is no longer active"
            );
            return Ok(());
        }
        if let Some(tab) = self.tabs.get_mut(owner) {
            tab.session.paste(text)?;
        }
        Ok(())
    }
}

fn selection_transfer_target(
    target: leyline_gfx::SelectionTarget,
) -> crate::clipboard::TransferTarget {
    match target {
        leyline_gfx::SelectionTarget::Clipboard => crate::clipboard::TransferTarget::Clipboard,
        leyline_gfx::SelectionTarget::Primary => crate::clipboard::TransferTarget::Primary,
    }
}

fn ignores_key_repeat(action: crate::config::Action) -> bool {
    matches!(
        action,
        crate::config::Action::NewTab
            | crate::config::Action::CloseTab
            | crate::config::Action::CopyClipboard
            | crate::config::Action::PasteClipboard
            | crate::config::Action::PastePrimary
    )
}

fn keep_selection_after_release(
    kind: Option<crate::terminal::SelectionKind>,
    dragged: bool,
) -> bool {
    dragged
        || matches!(
            kind,
            Some(crate::terminal::SelectionKind::Semantic | crate::terminal::SelectionKind::Lines)
        )
}

fn accumulate_wheel_steps(remainder_120: &mut i32, delta_120: i32) -> i32 {
    let total = remainder_120.saturating_add(delta_120);
    let steps = (total / 120).clamp(-4, 4);
    *remainder_120 = total % 120;
    steps
}

fn terminal_modifiers(key: &leyline_gfx::KeyInput) -> crate::terminal::Modifiers {
    crate::terminal::Modifiers {
        shift: key.modifiers.shift,
        control: key.modifiers.control
            && (!key.modifiers.alt_graph
                || key
                    .shortcut_modifiers
                    .contains(leyline_gfx::ModifierMask::CONTROL)),
        alt: key.modifiers.alt
            && (!key.modifiers.alt_graph
                || key
                    .shortcut_modifiers
                    .contains(leyline_gfx::ModifierMask::ALT)),
        super_key: key.modifiers.super_key,
    }
}

fn key_text(key: &leyline_gfx::KeyInput) -> Option<String> {
    key.utf8
        .as_ref()
        .filter(|text| !text.is_empty())
        .cloned()
        .or_else(|| match key.logical_key {
            leyline_gfx::LogicalKey::Character(ch) => Some(ch.to_string()),
            _ => None,
        })
}

fn select_new_tab_cwd(
    policy: &crate::config::NewTabCwdPolicy,
    source: Option<&crate::tab::TabEntry>,
    launch: &crate::app::LaunchContext,
) -> (std::path::PathBuf, &'static str) {
    match policy {
        crate::config::NewTabCwdPolicy::Inherit => {
            source.and_then(|tab| tab.cwd_hint.as_ref()).map_or_else(
                || (launch.base_cwd.clone(), "base"),
                |hint| (hint.path.clone(), "inherited"),
            )
        }
        crate::config::NewTabCwdPolicy::Fixed(path) => (path.clone(), "fixed"),
        crate::config::NewTabCwdPolicy::Home => launch.home.as_ref().map_or_else(
            || (launch.base_cwd.clone(), "base"),
            |home| (home.clone(), "home"),
        ),
    }
}

const fn cwd_policy_name(policy: &crate::config::NewTabCwdPolicy) -> &'static str {
    match policy {
        crate::config::NewTabCwdPolicy::Inherit => "inherit",
        crate::config::NewTabCwdPolicy::Fixed(_) => "fixed",
        crate::config::NewTabCwdPolicy::Home => "home",
    }
}

fn launch_title(launch: &crate::cli::LaunchRequest) -> String {
    match launch {
        crate::cli::LaunchRequest::DefaultShell => "Shell".into(),
        crate::cli::LaunchRequest::Command(command) => std::path::Path::new(&command.program)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("Command")
            .to_owned(),
    }
}

fn window_title(ordinal: usize, count: usize, title: &str) -> String {
    let prefix = format!("[{ordinal}/{count}] ");
    let suffix = " — Leyline";
    let available = leyline_gfx::MAX_WINDOW_TITLE_BYTES.saturating_sub(prefix.len() + suffix.len());
    let mut end = title.len().min(available);
    while !title.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{prefix}{}{suffix}", &title[..end])
}

#[cfg(test)]
mod tests {
    use super::{
        accumulate_wheel_steps, format_terminal_query, ignores_key_repeat,
        keep_selection_after_release, key_text, select_new_tab_cwd, terminal_modifiers,
    };

    #[test]
    fn new_tab_policy_selects_fixed_home_and_base_candidates() {
        let mut launch =
            crate::app::LaunchContext::for_test(crate::cli::LaunchRequest::DefaultShell);
        launch.base_cwd = "/base".into();
        launch.home = Some("/home/user".into());
        assert_eq!(
            select_new_tab_cwd(
                &crate::config::NewTabCwdPolicy::Fixed("/fixed".into()),
                None,
                &launch
            ),
            (std::path::PathBuf::from("/fixed"), "fixed")
        );
        assert_eq!(
            select_new_tab_cwd(&crate::config::NewTabCwdPolicy::Home, None, &launch),
            (std::path::PathBuf::from("/home/user"), "home")
        );
        launch.home = None;
        assert_eq!(
            select_new_tab_cwd(&crate::config::NewTabCwdPolicy::Home, None, &launch),
            (std::path::PathBuf::from("/base"), "base")
        );
        assert_eq!(
            select_new_tab_cwd(&crate::config::NewTabCwdPolicy::Inherit, None, &launch),
            (std::path::PathBuf::from("/base"), "base")
        );
    }

    #[test]
    fn plain_click_clears_selection_but_drag_and_multi_click_keep_it() {
        use crate::terminal::SelectionKind;

        assert!(!keep_selection_after_release(
            Some(SelectionKind::Simple),
            false
        ));
        assert!(keep_selection_after_release(
            Some(SelectionKind::Simple),
            true
        ));
        assert!(keep_selection_after_release(
            Some(SelectionKind::Semantic),
            false
        ));
        assert!(keep_selection_after_release(
            Some(SelectionKind::Lines),
            false
        ));
    }

    fn key(keysym: u32, utf8: Option<&str>) -> leyline_gfx::KeyInput {
        leyline_gfx::KeyInput {
            serial: leyline_gfx::InputSerial {
                seat: leyline_gfx::SeatToken::new(0, 1),
                value: 1,
                kind: leyline_gfx::SerialKind::Keyboard,
            },
            time_ms: 1,
            physical_keycode: 1,
            shortcut_digit_row: None,
            utf8: utf8.map(str::to_owned),
            modifiers: leyline_gfx::ModifiersState::default(),
            shortcut_modifiers: leyline_gfx::ModifierMask::empty(),
            logical_key: leyline_gfx::logical_key_from_keysym(keysym),
            state: leyline_gfx::KeyState::Pressed,
            repeat: false,
        }
    }

    #[test]
    fn wheel_accumulates_partial_steps_and_caps_each_event() {
        let mut remainder = 0;
        assert_eq!(accumulate_wheel_steps(&mut remainder, 40), 0);
        assert_eq!(accumulate_wheel_steps(&mut remainder, 80), 1);
        assert_eq!(remainder, 0);
        assert_eq!(accumulate_wheel_steps(&mut remainder, -60), 0);
        assert_eq!(accumulate_wheel_steps(&mut remainder, -60), -1);
        assert_eq!(accumulate_wheel_steps(&mut remainder, 1200), 4);
    }

    #[test]
    fn keyboard_text_falls_back_to_printable_keysym() {
        assert_eq!(key_text(&key(u32::from('a'), None)).as_deref(), Some("a"));
        assert_eq!(
            key_text(&key(u32::from('a'), Some(""))).as_deref(),
            Some("a")
        );
        assert_eq!(key_text(&key(0x0100_4e2d, None)).as_deref(), Some("中"));
        assert_eq!(key_text(&key(0xff51, None)), None);
    }

    #[test]
    fn shift_insert_uses_the_configurable_shortcut_matcher() {
        use crate::{
            config::{Action, KeyBinding},
            input::shortcut::{BindingOrigin, KeyChord, LogicalKeyPattern, ShortcutResult},
        };

        let mut input = key(0xff63, None);
        input.modifiers.shift = true;
        input.shortcut_modifiers = leyline_gfx::ModifierMask::SHIFT;
        let bindings = [KeyBinding {
            chord: KeyChord {
                key: LogicalKeyPattern::Insert,
                modifiers: leyline_gfx::ModifierMask::SHIFT,
            },
            action: Action::ScrollPageUp,
            origin: BindingOrigin::User { index: 0 },
        }];
        assert_eq!(
            crate::input::shortcut::resolve(&bindings, &input),
            ShortcutResult::Matched(Action::ScrollPageUp)
        );

        input.modifiers.control = true;
        input
            .shortcut_modifiers
            .insert(leyline_gfx::ModifierMask::CONTROL);
        assert_eq!(
            crate::input::shortcut::resolve(&bindings, &input),
            ShortcutResult::NotMatched
        );
    }

    #[test]
    fn selection_actions_ignore_key_repeat() {
        assert!(ignores_key_repeat(crate::config::Action::CopyClipboard));
        assert!(ignores_key_repeat(crate::config::Action::PasteClipboard));
        assert!(ignores_key_repeat(crate::config::Action::PastePrimary));
        assert!(!ignores_key_repeat(crate::config::Action::ScrollPageDown));
    }

    #[test]
    fn modifier_encoding_can_recover_the_unmodified_character() {
        assert_eq!(
            key(u32::from('c'), None).logical_key,
            leyline_gfx::LogicalKey::Character('c')
        );
        assert!(matches!(
            key(0x03, None).logical_key,
            leyline_gfx::LogicalKey::Unidentified(_)
        ));
    }

    #[test]
    fn alt_graph_text_does_not_become_a_control_alt_terminal_key() {
        let mut input = key(0x0100_20ac, Some("€"));
        input.modifiers.control = true;
        input.modifiers.alt = true;
        input.modifiers.alt_graph = true;
        assert_eq!(
            terminal_modifiers(&input),
            crate::terminal::Modifiers::default()
        );

        input
            .shortcut_modifiers
            .insert(leyline_gfx::ModifierMask::CONTROL);
        input
            .shortcut_modifiers
            .insert(leyline_gfx::ModifierMask::ALT);
        let modifiers = terminal_modifiers(&input);
        assert!(modifiers.control);
        assert!(modifiers.alt);
    }

    #[test]
    fn terminal_queries_use_config_colors_and_cell_grid_pixels() {
        use crate::terminal::{DefaultColorSlot, QueryTerminator, TerminalQuery};
        let geometry = crate::layout::TerminalGeometry {
            generation: 7,
            grid: crate::terminal::GridSize::new(80, 24).unwrap(),
            cell_px: [
                std::num::NonZeroU16::new(9).unwrap(),
                std::num::NonZeroU16::new(18).unwrap(),
            ],
        };
        assert_eq!(
            format_terminal_query(
                TerminalQuery::DefaultColor {
                    slot: DefaultColorSlot::Foreground,
                    terminator: QueryTerminator::StringTerminator,
                },
                geometry,
                0x1234_56aa,
                0x0102_03ff,
            )
            .unwrap(),
            b"\x1b]10;rgb:1212/3434/5656\x1b\\"
        );
        assert_eq!(
            format_terminal_query(
                TerminalQuery::DefaultColor {
                    slot: DefaultColorSlot::Background,
                    terminator: QueryTerminator::Bell,
                },
                geometry,
                0,
                0xaabb_cc01,
            )
            .unwrap(),
            b"\x1b]11;rgb:aaaa/bbbb/cccc\x07"
        );
        assert_eq!(
            format_terminal_query(TerminalQuery::TextAreaPixels, geometry, 0, 0).unwrap(),
            b"\x1b[4;432;720t"
        );
    }
}

fn earliest_timeout(render: Option<Duration>, deadline: Option<Instant>) -> Option<Duration> {
    let timer = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
    match (render, timer) {
        (Some(render), Some(timer)) => Some(render.min(timer)),
        (Some(render), None) => Some(render),
        (None, Some(timer)) => Some(timer),
        (None, None) => None,
    }
}

fn format_terminal_query(
    query: crate::terminal::TerminalQuery,
    geometry: TerminalGeometry,
    foreground: u32,
    background: u32,
) -> Option<Vec<u8>> {
    use crate::terminal::{DefaultColorSlot, QueryTerminator, TerminalQuery};

    let reply = match query {
        TerminalQuery::DefaultColor { slot, terminator } => {
            let (code, rgba) = match slot {
                DefaultColorSlot::Foreground => (10, foreground),
                DefaultColorSlot::Background => (11, background),
            };
            let [red, green, blue, _alpha] = rgba.to_be_bytes();
            let suffix = match terminator {
                QueryTerminator::Bell => "\x07",
                QueryTerminator::StringTerminator => "\x1b\\",
            };
            format!(
                "\x1b]{code};rgb:{red:02x}{red:02x}/{green:02x}{green:02x}/{blue:02x}{blue:02x}{suffix}"
            )
        }
        TerminalQuery::TextAreaPixels => {
            let width = u32::from(geometry.grid.columns.get())
                .checked_mul(u32::from(geometry.cell_px[0].get()))?;
            let height = u32::from(geometry.grid.lines.get())
                .checked_mul(u32::from(geometry.cell_px[1].get()))?;
            format!("\x1b[4;{height};{width}t")
        }
    };
    reply.is_ascii().then(|| reply.into_bytes())
}

impl WakeBackend for EventWake {
    fn wake(&self) {
        if let Err(error) = self.signal() {
            tracing::error!(category = "runtime", %error, "cannot signal UI eventfd");
        }
    }
}

fn tab_font_request(
    font_size: f64,
    scale_120: u32,
) -> Result<FontRequest, leyline_text::TextError> {
    FontRequest::from_points("sans-serif", (font_size * 0.86).max(8.0), scale_120, false).map(
        |request| {
            request
                .with_monospace(false)
                .with_rendering(HintingPreference::Full, AntialiasPreference::Grayscale)
        },
    )
}

fn text_hinting(value: crate::config::HintingPreference) -> HintingPreference {
    match value {
        crate::config::HintingPreference::None => HintingPreference::None,
        crate::config::HintingPreference::Slight => HintingPreference::Slight,
        crate::config::HintingPreference::Full => HintingPreference::Full,
        crate::config::HintingPreference::System => HintingPreference::System,
    }
}

fn text_antialiasing(value: crate::config::AntialiasPreference) -> AntialiasPreference {
    match value {
        crate::config::AntialiasPreference::Grayscale => AntialiasPreference::Grayscale,
        crate::config::AntialiasPreference::System => AntialiasPreference::System,
    }
}

fn content_insets(config: &crate::config::EffectiveConfig) -> ContentInsets {
    let right = if config.scrollbar.mode == crate::config::ScrollbarMode::Hidden {
        config.window.padding_x
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let gutter = config.scrollbar.hit_width.ceil() as u16;
        config.window.padding_x.max(gutter.saturating_add(2))
    };
    ContentInsets {
        left: config.window.padding_x,
        right,
        top: config
            .window
            .padding_y
            .saturating_add(config.tabs.bar_height),
        bottom: config.window.padding_y,
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
    SpawnDirectory(#[from] leyline_pty::SpawnDirectoryError),
    #[error(transparent)]
    Session(#[from] crate::session::SessionError),
    #[error(transparent)]
    Runtime(#[from] crate::app::runtime::RuntimeBuildError),
    #[error("cannot calculate terminal grid: {0}")]
    Grid(String),
    #[error(transparent)]
    Text(#[from] leyline_text::TextError),
    #[error(transparent)]
    Layout(#[from] crate::layout::LayoutError),
    #[error(transparent)]
    Compose(#[from] crate::frame_composer::ComposeError),
    #[error(transparent)]
    Ime(#[from] crate::interaction::ImeError),
}

impl ClassifiedError for UiRuntimeError {
    fn category(&self) -> ErrorCategory {
        match self {
            Self::Init(GfxInitError::Environment(_))
            | Self::SessionStart(_)
            | Self::SpawnDirectory(_)
            | Self::Graphics(GfxError::Platform(_)) => ErrorCategory::Environment,
            Self::Init(GfxInitError::Platform(_)) | Self::Ime(_) => ErrorCategory::Platform,
            Self::Init(GfxInitError::Device(_)) | Self::Graphics(GfxError::Renderer(_)) => {
                ErrorCategory::Renderer
            }
            Self::Graphics(GfxError::Internal(_))
            | Self::App(_)
            | Self::Wake(_)
            | Self::Session(_)
            | Self::Runtime(_)
            | Self::Grid(_)
            | Self::Text(_)
            | Self::Layout(_)
            | Self::Compose(_) => ErrorCategory::Internal,
        }
    }
}

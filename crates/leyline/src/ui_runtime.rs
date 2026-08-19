use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::{NonZeroU8, NonZeroU64},
    ops::{Deref, DerefMut},
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
        event::{ProcessSignal, ShutdownReason},
        runtime::{AppRuntime, AppRuntimeBuilder, WakeBackend},
    },
    diagnostics::{ClassifiedError, ErrorCategory},
    frame_composer::{FrameOverlays, compose, downgrade_color_working_set},
    interaction::{ClickTracker, ImeState, LinkCandidate, ScrollbarController, ScrollbarGeometry},
    layout::{ContentInsets, GridLayout, TerminalGeometry},
    session::{SessionAction, SessionReplyRequest, ShutdownPoll, TerminalSession},
    signal::ProcessSignalState,
    tab::TabManager,
    unicode_layout::{
        BuildStep, CaretAffinity, UnicodePolicy, VisualGridMap, VisualHit, VisualMapBuilder,
        begin_visual_map,
    },
};

struct PendingVisualBuild {
    snapshot: crate::terminal::FrameSnapshot,
    builder: VisualMapBuilder,
}

#[derive(Clone, Copy, Debug)]
struct SearchDialogDrag {
    grab_offset: [i64; 2],
}

#[derive(Clone, Copy, Debug)]
struct PressedTerminalKey {
    owner: crate::tab::SessionId,
    identity: leyline_gfx::KeyIdentity,
    associated_text_allowed: bool,
    foreground_process_group: Option<u32>,
}

struct DesktopGfx {
    host: leyline_gfx::GfxHost,
    current: leyline_gfx::WindowId,
}

impl DesktopGfx {
    fn adopt_initial(window: GfxRuntime, max_windows: NonZeroU8) -> Self {
        let (host, initial) = leyline_gfx::GfxHost::adopt_initial(window, max_windows);
        Self {
            host,
            current: initial,
        }
    }

    fn dispatch_pending(
        &mut self,
        output: &mut Vec<leyline_gfx::RoutedPlatformEvent>,
    ) -> Result<(), GfxError> {
        self.host.dispatch_pending(output)
    }

    fn create_window(&mut self, options: &GfxOptions) -> Result<leyline_gfx::WindowId, GfxError> {
        self.host.create_window(options)
    }

    fn accepts_surface(&self, surface: leyline_gfx::SurfaceKey) -> bool {
        self.host.accepts_surface(surface)
    }

    fn window(&self, id: leyline_gfx::WindowId) -> Option<&GfxRuntime> {
        self.host.window(id)
    }

    fn current_window(&self) -> &GfxRuntime {
        self.host
            .window(self.current)
            .expect("current graphics window is ready")
    }

    fn current_window_mut(&mut self) -> &mut GfxRuntime {
        self.host
            .window_mut(self.current)
            .expect("current graphics window is ready")
    }

    fn current_id(&self) -> leyline_gfx::WindowId {
        self.current
    }

    fn set_current(&mut self, id: leyline_gfx::WindowId) {
        self.current = id;
    }

    fn logical_size(&self) -> leyline_gfx::LogicalSize {
        self.current_window().logical_size()
    }

    fn scale(&self) -> leyline_gfx::Scale120 {
        self.current_window().scale()
    }

    fn color_glyphs_supported(&self) -> bool {
        self.current_window().color_glyphs_supported()
    }

    fn text_input_available(&self) -> bool {
        self.current_window().text_input_available()
    }

    fn apply(&mut self, command: leyline_gfx::GfxCommand) -> Result<(), GfxError> {
        self.current_window_mut().apply(command)
    }

    fn try_render(&mut self) -> Result<RenderOutcome, GfxError> {
        self.current_window_mut().try_render()
    }

    fn try_render_resize_preview(&mut self) -> Result<RenderOutcome, GfxError> {
        self.current_window_mut().try_render_resize_preview()
    }

    fn acknowledge_resize(&mut self) -> Result<(), GfxError> {
        self.current_window_mut().acknowledge_resize()
    }

    fn scene_fits_atlas(&self, scene: &leyline_gfx::SceneData) -> Result<bool, GfxError> {
        self.current_window().scene_fits_atlas(scene)
    }

    fn enable_text_input(
        &mut self,
        context: leyline_gfx::TextInputContext,
    ) -> Result<Option<u32>, GfxError> {
        self.current_window_mut().enable_text_input(context)
    }

    fn update_text_input(
        &mut self,
        context: leyline_gfx::TextInputContext,
    ) -> Result<Option<u32>, GfxError> {
        self.current_window_mut().update_text_input(context)
    }

    fn disable_text_input(&mut self) -> Result<Option<u32>, GfxError> {
        self.current_window_mut().disable_text_input()
    }

    fn publish_selection(
        &mut self,
        target: leyline_gfx::SelectionTarget,
        source: u64,
        serial: leyline_gfx::InputSerial,
    ) -> Result<bool, GfxError> {
        self.host
            .window_mut(self.current)
            .expect("desktop service owner is ready")
            .publish_selection(target, source, serial)
    }

    fn receive_selection(
        &mut self,
        target: leyline_gfx::SelectionTarget,
    ) -> Result<Option<std::os::fd::OwnedFd>, GfxError> {
        self.host
            .window_mut(self.current)
            .expect("desktop service owner is ready")
            .receive_selection(target)
    }

    fn remove_window(&mut self, id: leyline_gfx::WindowId) -> Result<(), GfxError> {
        self.host.remove_window(id)
    }

    fn next_creation_deadline(&self) -> Option<Instant> {
        self.host.next_creation_deadline()
    }

    fn expire_creating(&mut self, now: Instant) -> Vec<leyline_gfx::WindowId> {
        let mut expired = Vec::new();
        self.host.expire_creating(now, &mut expired);
        expired
    }

    fn poll_wait(
        &mut self,
        wake: Option<std::os::fd::BorrowedFd<'_>>,
        timeout: Option<Duration>,
    ) -> Result<(), GfxError> {
        self.host.poll_wait(wake, timeout)
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct DesktopRuntime {
    state: RuntimeState,
    app: App,
    gfx: DesktopGfx,
    wake: EventWake,
    current_window_id: leyline_gfx::WindowId,
    sessions: SessionRegistry,
    session_ids: crate::tab::SessionIdAllocator,
    wake_backend: Arc<dyn WakeBackend>,
    desktop_launcher: crate::desktop::DesktopLauncher,
    clipboard_workers: crate::clipboard::TransferWorkers,
    selection: crate::selection::SelectionController,
    notifications: crate::notification::NotificationWorker,
    sound: crate::sound::SoundWorker,
    windows: HashMap<leyline_gfx::WindowId, WindowRecord>,
    pending_platform_events: VecDeque<leyline_gfx::RoutedPlatformEvent>,
    window_service_cursor: Option<leyline_gfx::WindowId>,
    signal_state: Arc<ProcessSignalState>,
    signal_observed: bool,
    exit_signal: Option<ProcessSignal>,
}

pub enum DesktopRuntimeStart {
    Ready(Box<DesktopRuntime>),
    Signaled(ProcessSignal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiExit {
    Clean,
    Signaled(ProcessSignal),
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

#[derive(Clone, Copy, Debug)]
struct TabDragScroll {
    direction: i8,
    deadline: Instant,
}

enum WindowRecord {
    Creating {
        cwd: SpawnDirectory,
    },
    Moving {
        source: leyline_gfx::WindowId,
        session: crate::tab::SessionId,
    },
    Ready(Box<WindowRuntime>),
    Closing,
}

struct SessionRegistry {
    windows: HashMap<leyline_gfx::WindowId, TabManager>,
    locations: HashMap<crate::tab::SessionId, crate::window::SessionLocation>,
}

impl SessionRegistry {
    fn insert(&mut self, window: leyline_gfx::WindowId, tabs: TabManager) {
        for tab in tabs.tabs() {
            self.locations
                .insert(tab.id, crate::window::SessionLocation::Active { window });
        }
        assert!(self.windows.insert(window, tabs).is_none());
    }

    fn get(&self, window: leyline_gfx::WindowId) -> Option<&TabManager> {
        self.windows.get(&window)
    }

    fn get_mut(&mut self, window: leyline_gfx::WindowId) -> Option<&mut TabManager> {
        self.windows.get_mut(&window)
    }

    fn remove(&mut self, window: leyline_gfx::WindowId) -> Option<TabManager> {
        let tabs = self.windows.remove(&window)?;
        self.locations.retain(|_, location| match location {
            crate::window::SessionLocation::Active { window: owner }
            | crate::window::SessionLocation::Closing {
                former_window: owner,
            }
            | crate::window::SessionLocation::Moving { source: owner, .. } => *owner != window,
        });
        Some(tabs)
    }

    fn reconcile(&mut self, window: leyline_gfx::WindowId) {
        let Some(tabs) = self.windows.get(&window) else {
            return;
        };
        let live = tabs
            .tabs()
            .iter()
            .map(|tab| tab.id)
            .chain(tabs.closing().iter().map(|tab| tab.entry.id))
            .collect::<HashSet<_>>();
        self.locations.retain(|session, location| {
            let owner = match location {
                crate::window::SessionLocation::Active { window }
                | crate::window::SessionLocation::Closing {
                    former_window: window,
                }
                | crate::window::SessionLocation::Moving { source: window, .. } => *window,
            };
            owner != window || live.contains(session)
        });
    }

    fn owner(&self, session: crate::tab::SessionId) -> Option<leyline_gfx::WindowId> {
        match self.locations.get(&session) {
            Some(
                crate::window::SessionLocation::Active { window }
                | crate::window::SessionLocation::Moving { source: window, .. }
                | crate::window::SessionLocation::Closing {
                    former_window: window,
                },
            ) => Some(*window),
            None => None,
        }
    }

    fn mark_active(&mut self, session: crate::tab::SessionId, window: leyline_gfx::WindowId) {
        self.locations
            .insert(session, crate::window::SessionLocation::Active { window });
    }

    fn mark_closing(&mut self, session: crate::tab::SessionId, window: leyline_gfx::WindowId) {
        self.locations.insert(
            session,
            crate::window::SessionLocation::Closing {
                former_window: window,
            },
        );
    }

    fn commit_move_to_new_window(
        &mut self,
        source: leyline_gfx::WindowId,
        target: leyline_gfx::WindowId,
        session: crate::tab::SessionId,
        max_tabs: NonZeroU8,
    ) -> Result<(usize, bool), UiRuntimeError> {
        if self.windows.contains_key(&target) {
            return Err(crate::window::WindowError::DuplicateWindow(target).into());
        }
        if !self.windows.contains_key(&source) {
            return Err(crate::window::WindowError::UnknownWindow(source).into());
        }
        if self.locations.get(&session)
            != Some(&crate::window::SessionLocation::Active { window: source })
        {
            return Err(crate::window::WindowError::UnknownSession(session).into());
        }
        self.locations.insert(
            session,
            crate::window::SessionLocation::Moving {
                token: NonZeroU64::new(target.get()).expect("window ids are non-zero"),
                source,
            },
        );
        let extracted = self
            .windows
            .get_mut(&source)
            .expect("validated move source is registered")
            .extract(session);
        let (entry, from) = match extracted {
            Ok(value) => value,
            Err(error) => {
                self.mark_active(session, source);
                return Err(error.into());
            }
        };
        let source_empty = self.windows.get(&source).is_some_and(TabManager::is_empty);
        let previous = self
            .windows
            .insert(target, TabManager::bootstrap_entry(entry, max_tabs));
        assert!(previous.is_none(), "validated move target remains vacant");
        self.mark_active(session, target);
        Ok((from, source_empty))
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct WindowRuntime {
    text: TextSystem,
    tab_text: TextSystem,
    layout: GridLayout,
    layout_generation: u64,
    keyboard_focused: bool,
    tab_bar: crate::tab::TabBarPresentation,
    window_state: crate::window::WindowStateController,
    tab_drag: Option<crate::tab::TabDrag>,
    tab_drag_scroll: Option<TabDragScroll>,
    text_scale: leyline_gfx::Scale120,
    resize_settle_deadline: Option<Instant>,
    font_size: f64,
    reset_font_size: f64,
    modifiers: leyline_gfx::ModifiersState,
    terminal_control_gesture: bool,
    local_pressed_keys: HashSet<(leyline_gfx::SeatToken, u32)>,
    terminal_pressed_keys: HashMap<(leyline_gfx::SeatToken, u32), PressedTerminalKey>,
    cursor_blink_visible: bool,
    cursor_blink_deadline: Option<Instant>,
    selecting: bool,
    selection_point: Option<crate::terminal::SelectionPoint>,
    selection_kind: Option<crate::terminal::SelectionKind>,
    selection_dragged: bool,
    click_tracker: ClickTracker,
    drag_scroll: Option<DragScroll>,
    link_candidate: Option<LinkCandidate>,
    ime: ImeState,
    ime_context: Option<leyline_gfx::TextInputContext>,
    pending_search_paste: Option<(crate::tab::SessionId, u64)>,
    last_input_serial: Option<leyline_gfx::InputSerial>,
    wheel_remainder_120: i32,
    scrollbar: ScrollbarController,
    search_dialog: Option<crate::search::SearchDialogPresentation>,
    search_focused: bool,
    search_dialog_origin: Option<[u32; 2]>,
    search_dialog_drag: Option<SearchDialogDrag>,
    visual_build: Option<PendingVisualBuild>,
    pending_visual_map: Option<(leyline_gfx::FrameKey, Arc<VisualGridMap>)>,
    published_visual_map: Option<Arc<VisualGridMap>>,
    visual_bell: crate::bell::VisualBellState,
}

impl WindowRuntime {
    fn ready(
        text: TextSystem,
        tab_text: TextSystem,
        layout: GridLayout,
        scale: leyline_gfx::Scale120,
        font_size: f64,
        desired_state: crate::window::DesiredWindowState,
    ) -> Self {
        Self {
            text,
            tab_text,
            layout,
            layout_generation: 1,
            keyboard_focused: false,
            tab_bar: crate::tab::TabBarPresentation::default(),
            window_state: crate::window::WindowStateController::new(desired_state),
            tab_drag: None,
            tab_drag_scroll: None,
            text_scale: scale,
            resize_settle_deadline: None,
            font_size,
            reset_font_size: font_size,
            modifiers: leyline_gfx::ModifiersState::default(),
            terminal_control_gesture: false,
            local_pressed_keys: HashSet::new(),
            terminal_pressed_keys: HashMap::new(),
            cursor_blink_visible: true,
            cursor_blink_deadline: None,
            selecting: false,
            selection_point: None,
            selection_kind: None,
            selection_dragged: false,
            click_tracker: ClickTracker::default(),
            drag_scroll: None,
            link_candidate: None,
            ime: ImeState::default(),
            ime_context: None,
            pending_search_paste: None,
            last_input_serial: None,
            wheel_remainder_120: 0,
            scrollbar: ScrollbarController::default(),
            search_dialog: None,
            search_focused: false,
            search_dialog_origin: None,
            search_dialog_drag: None,
            visual_build: None,
            pending_visual_map: None,
            published_visual_map: None,
            visual_bell: crate::bell::VisualBellState::default(),
        }
    }
}

impl Deref for DesktopRuntime {
    type Target = WindowRuntime;

    fn deref(&self) -> &Self::Target {
        match self.windows.get(&self.current_window_id) {
            Some(WindowRecord::Ready(window)) => window,
            _ => panic!("current window is not ready"),
        }
    }
}

impl DerefMut for DesktopRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self.windows.get_mut(&self.current_window_id) {
            Some(WindowRecord::Ready(window)) => window,
            _ => panic!("current window is not ready"),
        }
    }
}

struct WindowPresentation {
    text: TextSystem,
    tab_text: TextSystem,
    layout: GridLayout,
}

const DRAG_SCROLL_INTERVAL: Duration = Duration::from_millis(50);
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(25);
const RESIZE_SETTLE_INTERVAL: Duration = Duration::from_millis(50);
const WINDOW_STATE_STABILIZATION: Duration = Duration::from_secs(1);
const CLIPBOARD_RESULT_BUDGET: usize = 2;
const MAX_PRESSED_KEYS: usize = 256;
const MAX_PROCESS_SESSIONS: usize = 128;
const PROCESS_UI_TIME_BUDGET: Duration = Duration::from_millis(8);
const PROCESS_PLATFORM_EVENT_BUDGET: usize = 256;

impl DesktopRuntime {
    fn current_tabs(&self) -> &TabManager {
        self.sessions
            .get(self.current_window_id)
            .expect("current window sessions are registered")
    }

    fn current_tabs_mut(&mut self) -> &mut TabManager {
        self.sessions
            .get_mut(self.current_window_id)
            .expect("current window sessions are registered")
    }

    fn activate_window(&mut self, id: leyline_gfx::WindowId) -> bool {
        if id == self.current_window_id {
            return true;
        }
        if !matches!(self.windows.get(&id), Some(WindowRecord::Ready(_))) {
            return false;
        }
        self.current_window_id = id;
        self.gfx.set_current(id);
        true
    }

    fn handle_routed_platform_event(
        &mut self,
        routed: leyline_gfx::RoutedPlatformEvent,
    ) -> Result<bool, UiRuntimeError> {
        if !self.gfx.accepts_surface(routed.surface) {
            tracing::debug!(
                category = "stale_platform_event",
                window_id = routed.surface.window.get(),
                surface_generation = routed.surface.generation.get(),
                "discarded event for stale surface"
            );
            return Ok(false);
        }
        let window = routed.surface.window;
        if window != self.current_window_id
            && matches!(self.windows.get(&window), Some(WindowRecord::Ready(_)))
        {
            self.activate_window(window);
        }
        if window == self.current_window_id {
            self.handle_window_event(routed.event)
        } else {
            self.handle_pending_window_event(window, &routed.event)?;
            Ok(false)
        }
    }

    fn try_render_current_isolated(
        &mut self,
        resize_preview: bool,
    ) -> Result<Option<RenderOutcome>, UiRuntimeError> {
        let result = if resize_preview {
            self.gfx.try_render_resize_preview()
        } else {
            self.gfx.try_render()
        };
        match result {
            Ok(outcome) => Ok(Some(outcome)),
            Err(GfxError::Renderer(fault)) if renderer_fault_is_process_fatal(&fault) => {
                Err(GfxError::Renderer(fault).into())
            }
            Err(GfxError::Renderer(fault)) => {
                let failed = self.current_window_id;
                tracing::error!(
                    category = "window_renderer_failed",
                    current_window_id = failed.get(),
                    %fault,
                    "closing only the failed rendering surface"
                );
                self.close_current_window()?;
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn activate_session_owner(&mut self, session: crate::tab::SessionId) -> bool {
        if self.current_tabs_mut().get_mut(session).is_some() {
            return true;
        }
        let owner = self.sessions.owner(session);
        owner.is_some_and(|owner| self.activate_window(owner))
    }

    fn active_session(&self) -> &TerminalSession {
        &self
            .current_tabs()
            .active()
            .expect("running UI has an active tab")
            .session
    }

    fn active_session_mut(&mut self) -> &mut TerminalSession {
        &mut self
            .current_tabs_mut()
            .active_mut()
            .expect("running UI has an active tab")
            .session
    }

    #[allow(clippy::too_many_lines)]
    fn drain_sessions(&mut self, process_deadline: Instant) -> Result<(), UiRuntimeError> {
        const WINDOW_EVENT_BUDGET: usize = 64;
        const WINDOW_BYTE_BUDGET: usize = 1024 * 1024;
        const WINDOW_TIME_BUDGET: Duration = Duration::from_millis(2);
        let started = Instant::now();
        let drain_deadline = (started + WINDOW_TIME_BUDGET).min(process_deadline);
        let mut events = 0_usize;
        let mut bytes = 0_usize;
        let mut completed = Vec::new();
        let mut tab_presentation_changed = false;
        let fallback_title = launch_title(self.app.launch());
        let local_identity = self.app.launch_context().local_identity.clone();
        let geometry = self.layout.terminal_geometry(self.layout_generation);
        let foreground = self.app.config().colors.foreground.0;
        let background = self.app.config().colors.background.0;
        for id in self.current_tabs_mut().drain_order() {
            let is_active = Some(id) == self.current_tabs().active_id();
            let mut incoming = Vec::new();
            let result = self
                .current_tabs_mut()
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
                let output_activity = matches!(
                    &pty,
                    crate::app::event::PtyEvent::Output(batch)
                        if !batch.as_slice().iter().all(|byte| *byte == b'\x07')
                );
                let action = match self
                    .current_tabs_mut()
                    .get_mut(id)
                    .expect("drain id exists")
                    .session
                    .handle_pty_event(pty)
                {
                    Ok(action) => action,
                    Err(error) => {
                        let tab = self
                            .current_tabs_mut()
                            .get_mut(id)
                            .expect("drain id exists");
                        tab.session.mark_failed();
                        tab.runtime.fast_cancel();
                        tracing::warn!(category = "tab_session_failed", session_id = id.get(), %error, "tab session failed");
                        continue;
                    }
                };
                if output_activity && id != self.current_tabs().active_id().expect("active tab") {
                    tab_presentation_changed |= self.current_tabs_mut().mark_unread(id);
                }
                if matches!(action, SessionAction::Completed) {
                    completed.push(id);
                }
            }
            let window_focused = self.keyboard_focused;
            let visual_duration = self.app.config().bell.visual_duration;
            let bell_config = self.app.config().bell.clone();
            let tab = self
                .current_tabs_mut()
                .get_mut(id)
                .expect("drain id exists");
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
                match self
                    .current_tabs_mut()
                    .apply_cwd_report(id, report, &local_identity)
                {
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
            let tab = self
                .current_tabs_mut()
                .get_mut(id)
                .expect("drain id exists");
            if let Some(title) = tab.session.take_title() {
                tab_presentation_changed |=
                    update_session_title(&mut tab.title, title, &fallback_title);
            }
            let bell = tab.session.take_bell();
            if bell {
                let allowed = !completed.contains(&id) && tab.session.bell_effects_allowed();
                let muted = tab.bell_muted;
                let effects = crate::bell::decide(
                    crate::bell::BellContext {
                        session_id: id,
                        active: is_active,
                        window_focused,
                        muted,
                        session_effects_allowed: allowed,
                    },
                    &bell_config,
                );
                let generation = if effects.record_attention_episode {
                    let was_attention = tab.attention;
                    let generation = self
                        .current_tabs_mut()
                        .record_background_bell(id, effects.show_attention_marker)?;
                    tab_presentation_changed |= effects.show_attention_marker && !was_attention;
                    Some(generation)
                } else {
                    None
                };
                if effects.schedule_visual {
                    self.visual_bell
                        .schedule(id, Instant::now(), visual_duration);
                    tab_presentation_changed = true;
                }
                if effects.enqueue_notification {
                    let ordinal = self
                        .current_tabs_mut()
                        .tabs()
                        .iter()
                        .position(|tab| tab.id == id)
                        .and_then(|index| u8::try_from(index + 1).ok())
                        .unwrap_or(1);
                    if let Some(generation) = generation {
                        let _ = self.notifications.show(
                            id,
                            generation,
                            ordinal,
                            Instant::now(),
                            &bell_config,
                        );
                    }
                }
                if effects.enqueue_sound {
                    let _ = self.sound.play(id);
                }
            }
            if events >= WINDOW_EVENT_BUDGET
                || bytes >= WINDOW_BYTE_BUDGET
                || (events > 0 && Instant::now() >= drain_deadline)
            {
                self.wake.signal()?;
                break;
            }
        }
        for id in completed {
            if self.current_tabs().is_empty() {
                break;
            }
            if self.current_tabs().active_id() != Some(id) {
                let _ = self.current_tabs_mut().activate(id);
            }
            self.close_active_tab(ShutdownReason::ChildExited)?;
        }
        self.current_tabs_mut().poll_closing(Instant::now())?;
        self.sessions.reconcile(self.current_window_id);
        if tab_presentation_changed && !self.current_tabs().is_empty() {
            self.compose_latest()?;
        }
        Ok(())
    }

    fn refresh_active_title(&mut self) -> Result<(), UiRuntimeError> {
        let fallback = launch_title(self.app.launch());
        if let Some(title) = self.active_session_mut().take_title() {
            self.current_tabs_mut()
                .active_mut()
                .expect("active tab")
                .title = resolved_session_title(title, &fallback);
        }
        let active = self.current_tabs().active().expect("active tab");
        let ordinal = self
            .current_tabs()
            .tabs()
            .iter()
            .position(|tab| tab.id == active.id)
            .unwrap_or(0)
            + 1;
        let title = window_title(ordinal, self.current_tabs().len(), &active.title);
        self.gfx.apply(leyline_gfx::GfxCommand::SetTitle(title))?;
        Ok(())
    }

    /// Builds the single UI-thread composition root.
    ///
    /// # Errors
    /// Returns a typed graphics initialization failure.
    #[allow(clippy::too_many_lines)]
    pub fn initialize(
        mut app: App,
        app_runtime: AppRuntime,
        wake: EventWake,
        signal_state: Arc<ProcessSignalState>,
    ) -> Result<DesktopRuntimeStart, UiRuntimeError> {
        let clear = LinearColor::from_srgba8(app.config().colors.background.0);
        let bootstrap_request = FontRequest::from_points(
            app.config().font.family.clone(),
            app.config().font.size,
            leyline_gfx::Scale120::ONE.0,
            app.config().font.ligatures,
        )?
        .with_rendering(
            text_hinting(app.config().font.hinting),
            text_antialiasing(app.config().font.antialiasing),
        );
        let bootstrap_text = TextSystem::new(bootstrap_request)?;
        let default_size = requested_normal_size(app.config(), bootstrap_text.metrics())?;
        let mut gfx = GfxRuntime::new(&GfxOptions {
            clear,
            default_size,
            ..GfxOptions::default()
        })?;
        let mut desired_window_state = crate::window::DesiredWindowState::default();
        match app.config().window.startup_state {
            crate::config::StartupWindowState::Normal => {}
            crate::config::StartupWindowState::Maximized => {
                desired_window_state.maximized = true;
                gfx.apply(leyline_gfx::GfxCommand::RequestMaximized(true))?;
            }
            crate::config::StartupWindowState::Fullscreen => {
                desired_window_state.fullscreen = true;
                gfx.apply(leyline_gfx::GfxCommand::RequestFullscreen(true))?;
            }
        }
        let gfx = DesktopGfx::adopt_initial(gfx, app.config().window.max_windows);
        let request = FontRequest::from_points(
            app.config().font.family.clone(),
            app.config().font.size,
            gfx.scale().0,
            app.config().font.ligatures,
        )?
        .with_rendering(
            text_hinting(app.config().font.hinting),
            text_antialiasing(app.config().font.antialiasing),
        )
        .with_color_glyphs(app.config().unicode.color_glyphs && gfx.color_glyphs_supported());
        let text = TextSystem::new(request)?;
        let tab_request = tab_font_request(app.config().font.size, gfx.scale().0)?;
        let tab_text = TextSystem::new(tab_request)?;
        tracing::info!(
            category = "unicode_profile",
            bidi = app.config().unicode.bidi,
            color_glyphs = app.config().unicode.color_glyphs && gfx.color_glyphs_supported(),
            unicode_bidi = crate::unicode_layout::UNICODE_BIDI_VERSION,
            unicode_version = "16.0.0",
            "resolved Unicode layout profile"
        );
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
                gray_atlas_format = "R8_UNORM",
                color_atlas_format = "R8G8B8A8_SRGB",
                color_glyphs = app.config().unicode.color_glyphs && gfx.color_glyphs_supported(),
                atlas_filter = "nearest",
                "resolved terminal text profile"
            );
        }
        let layout = GridLayout::calculate_with_style(
            gfx.logical_size(),
            gfx.scale(),
            content_insets(app.config(), 1),
            text.metrics(),
            app.config().font.line_spacing,
            text.generation(),
        )?;
        let initial_size = layout.grid;
        let text_scale = gfx.scale();
        if let Some(signal) = signal_state.first_signal() {
            app.request_shutdown(ShutdownReason::Signal(signal))?;
            app.stop()?;
            tracing::info!(
                category = "application_shutdown_complete",
                signal = signal.number(),
                received_count = signal_state.received_count(),
                "process signal arrived before the first PTY was spawned"
            );
            return Ok(DesktopRuntimeStart::Signaled(signal));
        }
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
        let mut session_ids = crate::tab::SessionIdAllocator::default();
        let initial_session_id = session_ids.allocate()?;
        let tabs =
            TabManager::bootstrap_with_id(initial_session_id, session, app_runtime, max_count);
        let wake_backend: Arc<dyn WakeBackend> = Arc::new(wake.clone());
        let reset_font_size = app.config().font.size;
        let clipboard_workers = crate::clipboard::TransferWorkers::new(&wake);
        let current_window_id = gfx.current_id();
        let initial_window = WindowRuntime {
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
            window_state: crate::window::WindowStateController::new(desired_window_state),
            local_pressed_keys: HashSet::new(),
            terminal_pressed_keys: HashMap::new(),
            cursor_blink_visible: true,
            cursor_blink_deadline: None,
            selecting: false,
            selection_point: None,
            selection_kind: None,
            selection_dragged: false,
            click_tracker: ClickTracker::default(),
            drag_scroll: None,
            link_candidate: None,
            ime: ImeState::default(),
            ime_context: None,
            pending_search_paste: None,
            last_input_serial: None,
            wheel_remainder_120: 0,
            scrollbar: ScrollbarController::default(),
            tab_bar: crate::tab::TabBarPresentation::default(),
            tab_drag: None,
            tab_drag_scroll: None,
            search_dialog: None,
            search_focused: false,
            search_dialog_origin: None,
            search_dialog_drag: None,
            visual_build: None,
            pending_visual_map: None,
            published_visual_map: None,
            visual_bell: crate::bell::VisualBellState::default(),
        };
        let windows = HashMap::from([(
            current_window_id,
            WindowRecord::Ready(Box::new(initial_window)),
        )]);
        let sessions = SessionRegistry {
            windows: HashMap::from([(current_window_id, tabs)]),
            locations: HashMap::from([(
                initial_session_id,
                crate::window::SessionLocation::Active {
                    window: current_window_id,
                },
            )]),
        };
        Ok(DesktopRuntimeStart::Ready(Box::new(Self {
            state: RuntimeState::Running,
            app,
            gfx,
            wake,
            current_window_id,
            sessions,
            session_ids,
            wake_backend,
            desktop_launcher: crate::desktop::DesktopLauncher::new(),
            clipboard_workers,
            selection: crate::selection::SelectionController::default(),
            notifications: crate::notification::NotificationWorker::new(),
            sound: crate::sound::SoundWorker::new(),
            windows,
            pending_platform_events: VecDeque::new(),
            window_service_cursor: None,
            signal_state,
            signal_observed: false,
            exit_signal: None,
        })))
    }

    /// Runs the demand-driven window loop until the compositor requests close.
    ///
    /// # Errors
    /// Returns a typed platform, renderer, or application failure.
    pub fn run(mut self) -> Result<UiExit, UiRuntimeError> {
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
    fn run_loop(&mut self) -> Result<UiExit, UiRuntimeError> {
        loop {
            self.drain_process_signal()?;
            if let Some(signal) = self.exit_signal {
                self.finish_process_shutdown()?;
                self.app.stop()?;
                tracing::info!(
                    category = "application_shutdown_complete",
                    signal = signal.number(),
                    received_count = self.signal_state.received_count(),
                    wake_failed = self.signal_state.wake_failed(),
                    "signal-triggered application shutdown completed"
                );
                return Ok(UiExit::Signaled(signal));
            }
            let round_deadline = Instant::now() + PROCESS_UI_TIME_BUDGET;
            if self.pending_platform_events.is_empty() {
                let mut events = Vec::new();
                self.gfx.dispatch_pending(&mut events)?;
                self.pending_platform_events.extend(events);
            }
            while let Some(routed) = take_priority_close_event(&mut self.pending_platform_events) {
                let _ = self.handle_routed_platform_event(routed)?;
            }
            let mut platform_events = 0_usize;
            while let Some(routed) = self.pending_platform_events.pop_front() {
                if self.handle_routed_platform_event(routed)? {
                    break;
                }
                platform_events = platform_events.saturating_add(1);
                if platform_events >= PROCESS_PLATFORM_EVENT_BUDGET
                    || Instant::now() >= round_deadline
                {
                    if !self.pending_platform_events.is_empty() {
                        self.wake.signal()?;
                    }
                    break;
                }
            }
            for id in self.gfx.expire_creating(Instant::now()) {
                match self.windows.remove(&id) {
                    Some(WindowRecord::Moving { session, .. }) => tracing::warn!(
                        category = "tab_move_rolled_back",
                        current_window_id = id.get(),
                        session_id = session.get(),
                        reason = "initial_configure_timeout",
                        "tab move target timed out"
                    ),
                    Some(WindowRecord::Creating { .. }) => tracing::warn!(
                        category = "window_create_failed",
                        current_window_id = id.get(),
                        reason = "initial_configure_timeout",
                        "new window creation timed out"
                    ),
                    Some(WindowRecord::Ready(_) | WindowRecord::Closing) | None => {}
                }
            }
            if self.service_background_windows(round_deadline)? {
                self.wake.signal()?;
                continue;
            }
            if Instant::now() >= round_deadline {
                self.wake.signal()?;
                continue;
            }
            self.apply_settled_resize()?;
            if self.window_state.expire(Instant::now()) {
                tracing::debug!(
                    category = "window_state_configured",
                    effective = ?self.window_state.effective(),
                    "window state request stabilized at compositor state"
                );
            }
            self.flush_expired_sync(Instant::now())?;
            self.drain_sessions(round_deadline)?;
            self.notifications.retry_controls();
            if self.current_tabs().is_empty() {
                if self.windows.iter().any(|(id, window)| {
                    *id != self.current_window_id && matches!(window, WindowRecord::Ready(_))
                }) {
                    self.close_current_window()?;
                    continue;
                }
                self.finish_last_tab_shutdown()?;
                break;
            }
            self.drain_clipboard_results()?;
            self.process_drag_scroll()?;
            self.process_tab_drag_scroll()?;
            if self.scrollbar.expire(Instant::now()) {
                self.compose_latest()?;
            }
            if self.visual_bell.expire(Instant::now()) {
                self.compose_latest()?;
            }
            let search_effect = self.active_session_mut().advance_search(Instant::now())?;
            if search_effect.needs_frame && search_effect.scroll_target.is_none() {
                self.compose_latest()?;
            }
            if let Some(snapshot) = self.active_session_mut().end_drain_round()? {
                self.compose_snapshot(&snapshot)?;
            }
            self.advance_visual_build()?;
            self.advance_cursor_blink(Instant::now())?;
            self.refresh_active_title()?;
            if self.poll_shutdown()? {
                break;
            }
            let resize_preview = self.resize_settle_deadline.is_some();
            let Some(render_outcome) = self.try_render_current_isolated(resize_preview)? else {
                continue;
            };
            let render_timeout = match render_outcome {
                RenderOutcome::Deferred => Some(GfxRuntime::retry_delay()),
                RenderOutcome::Rendered { committed } => {
                    if let Some(map) =
                        take_matching_pending(&mut self.pending_visual_map, committed)
                    {
                        let mapping_changed =
                            visual_mapping_changed(self.published_visual_map.as_deref(), &map);
                        self.published_visual_map = Some(map);
                        if mapping_changed {
                            self.cancel_pointer_gesture();
                        }
                        self.refresh_text_input_rectangle()?;
                    }
                    None
                }
                RenderOutcome::WaitingForFrame | RenderOutcome::Idle => None,
            };
            let shutdown_poll = self
                .current_tabs()
                .tabs()
                .iter()
                .filter_map(|tab| tab.session.shutdown_deadline())
                .chain(self.current_tabs().next_closing_deadline())
                .min()
                .map(|deadline| deadline.min(Instant::now() + SHUTDOWN_POLL_INTERVAL));
            let sync_deadline = self
                .current_tabs_mut()
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
            let timeout =
                earliest_timeout(timeout, self.tab_drag_scroll.map(|scroll| scroll.deadline));
            let timeout = earliest_timeout(timeout, self.scrollbar.next_deadline());
            let timeout = earliest_timeout(timeout, sync_deadline);
            let timeout = earliest_timeout(timeout, self.cursor_blink_deadline);
            let timeout = earliest_timeout(timeout, self.visual_bell.deadline());
            let timeout = earliest_timeout(timeout, self.active_session().search().next_deadline());
            let timeout = earliest_timeout(
                timeout,
                self.window_state.pending().map(|pending| pending.deadline),
            );
            let timeout = earliest_timeout(timeout, self.gfx.next_creation_deadline());
            let window_shutdown = self
                .sessions
                .windows
                .values()
                .filter_map(TabManager::next_closing_deadline)
                .min();
            let timeout = earliest_timeout(timeout, window_shutdown);
            let window_window_state = self
                .windows
                .values()
                .filter_map(|window| match window {
                    WindowRecord::Ready(window) => window
                        .window_state
                        .pending()
                        .map(|pending| pending.deadline),
                    WindowRecord::Creating { .. }
                    | WindowRecord::Moving { .. }
                    | WindowRecord::Closing => None,
                })
                .min();
            let timeout = earliest_timeout(timeout, window_window_state);
            let window_tab_drag_scroll = self
                .windows
                .values()
                .filter_map(|window| match window {
                    WindowRecord::Ready(window) => {
                        window.tab_drag_scroll.map(|scroll| scroll.deadline)
                    }
                    WindowRecord::Creating { .. }
                    | WindowRecord::Moving { .. }
                    | WindowRecord::Closing => None,
                })
                .min();
            let timeout = earliest_timeout(timeout, window_tab_drag_scroll);
            let timeout = if self.visual_build.is_some() {
                Some(Duration::ZERO)
            } else {
                timeout
            };
            if self
                .current_tabs_mut()
                .tabs()
                .iter()
                .all(|tab| tab.runtime.inbox_ref().prepare_to_wait())
            {
                self.gfx.poll_wait(Some(self.wake.as_fd()), timeout)?;
                self.wake.drain()?;
            }
        }
        self.app.stop()?;
        Ok(UiExit::Clean)
    }

    fn drain_process_signal(&mut self) -> Result<(), UiRuntimeError> {
        if self.signal_observed {
            return Ok(());
        }
        let Some(signal) = self.signal_state.first_signal() else {
            return Ok(());
        };
        self.signal_observed = true;
        tracing::info!(
            category = "process_signal_received",
            signal = signal.number(),
            "process signal relayed to the UI lifecycle"
        );
        let transition = self.app.request_shutdown(ShutdownReason::Signal(signal))?;
        if transition == crate::app::ShutdownTransition::Started
            || matches!(
                self.app.lifecycle(),
                crate::app::Lifecycle::ShuttingDown(ShutdownReason::Signal(existing))
                    if *existing == signal
            )
        {
            self.exit_signal = Some(signal);
            self.begin_all_session_shutdown("signal");
        }
        Ok(())
    }

    fn begin_all_session_shutdown(&mut self, reason: &'static str) {
        for tabs in self.sessions.windows.values_mut() {
            for tab in tabs.tabs_mut() {
                tab.runtime.fast_cancel();
                tab.session.begin_shutdown();
            }
        }
        tracing::info!(
            category = "application_shutdown_started",
            reason,
            sessions = self.process_session_count(),
            "all terminal sessions entered shutdown"
        );
    }

    fn finish_process_shutdown(&mut self) -> Result<(), UiRuntimeError> {
        loop {
            let now = Instant::now();
            let mut pending = false;
            let mut timed_out = false;
            for tabs in self.sessions.windows.values_mut() {
                tabs.poll_closing(now)?;
                pending |= !tabs.closing_is_empty();
                for tab in tabs.tabs_mut() {
                    if tab.session.shutdown_deadline().is_none() {
                        continue;
                    }
                    match tab.session.poll_shutdown(now)? {
                        ShutdownPoll::Pending => pending = true,
                        ShutdownPoll::TimedOut => timed_out = true,
                        ShutdownPoll::Complete => {}
                    }
                }
            }
            if timed_out {
                tracing::warn!(
                    category = "pty",
                    module = "ui_runtime",
                    "PTY shutdown exceeded the 2 second completion deadline; detached owned workers"
                );
            }
            if !pending {
                return Ok(());
            }
            if let Err(error) = self
                .gfx
                .poll_wait(Some(self.wake.as_fd()), Some(SHUTDOWN_POLL_INTERVAL))
            {
                tracing::warn!(category = "shutdown_poll", %error, "platform poll failed during bounded shutdown");
                std::thread::sleep(SHUTDOWN_POLL_INTERVAL);
            } else if let Err(error) = self.wake.drain() {
                tracing::warn!(category = "shutdown_poll", %error, "event wake drain failed during bounded shutdown");
            }
        }
    }

    /// Routes one event for the bootstrap window through the same tagged host stream as every
    /// other surface. Returning true stops the current dispatch batch after the window closes.
    fn handle_window_event(&mut self, event: PlatformEvent) -> Result<bool, UiRuntimeError> {
        match event {
            PlatformEvent::CloseRequested => {
                self.close_current_window()?;
                return Ok(true);
            }
            PlatformEvent::Configured { state, .. } => {
                self.window_state.configured(state);
                self.cancel_pointer_gesture();
                self.resize_settle_deadline = Some(Instant::now() + RESIZE_SETTLE_INTERVAL);
                self.gfx.acknowledge_resize()?;
            }
            PlatformEvent::ScaleChanged { .. } => {
                self.cancel_pointer_gesture();
                self.resize_settle_deadline = Some(Instant::now() + RESIZE_SETTLE_INTERVAL);
                self.gfx.acknowledge_resize()?;
            }
            PlatformEvent::KeyboardFocus { focused, .. } => {
                self.keyboard_focused = focused;
                self.visual_bell.cancel();
                if focused
                    && let Some(id) = self.current_tabs().active_id()
                    && let Some(generation) = self.current_tabs_mut().acknowledge(id)
                {
                    self.notifications.acknowledge(id, generation);
                }
                self.active_session_mut().focus_changed(focused)?;
                if !focused {
                    self.local_pressed_keys.clear();
                    self.terminal_pressed_keys.clear();
                    self.terminal_control_gesture = false;
                    self.cancel_paste_confirmation()?;
                    self.cancel_pointer_gesture();
                    if let Some(serial) = self.gfx.disable_text_input()? {
                        self.ime.record_commit_serial(serial);
                    }
                    self.ime.deactivate();
                    self.ime_context = None;
                }
                self.compose_latest()?;
            }
            PlatformEvent::Key(key) => self.handle_key(&key)?,
            PlatformEvent::ModifiersChanged(modifiers) => self.modifiers_changed(modifiers),
            PlatformEvent::Pointer(pointer) => self.handle_pointer(pointer)?,
            PlatformEvent::TextInput(event) => self.handle_text_input(event)?,
            PlatformEvent::Clipboard(event) => self.handle_clipboard_event(event)?,
            PlatformEvent::SurfaceSuspended => self.cancel_pointer_gesture(),
            PlatformEvent::FrameReady | PlatformEvent::SurfaceResumed => {}
        }
        Ok(false)
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
        let _ = self.app.request_shutdown(ShutdownReason::PlatformFailure);
        self.begin_all_session_shutdown("platform_failure");
        let _ = self.finish_process_shutdown();
        let _ = self.app.stop();
    }

    fn poll_shutdown(&mut self) -> Result<bool, UiRuntimeError> {
        self.current_tabs_mut().poll_closing(Instant::now())?;
        if matches!(self.app.lifecycle(), crate::app::Lifecycle::ShuttingDown(_))
            && self.current_tabs().is_empty()
            && self.current_tabs().closing_is_empty()
        {
            return Ok(true);
        }
        if self
            .current_tabs_mut()
            .tabs()
            .iter()
            .all(|tab| tab.session.shutdown_deadline().is_none())
        {
            return Ok(false);
        }
        let mut pending = false;
        let mut timed_out = false;
        for tab in self.current_tabs_mut().tabs_mut() {
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
            for tab in self.current_tabs_mut().tabs_mut() {
                tab.runtime.fast_cancel();
            }
            Ok(true)
        }
    }

    #[allow(clippy::too_many_lines)]
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
        )
        .with_color_glyphs(
            self.app.config().unicode.color_glyphs && self.gfx.color_glyphs_supported(),
        );
        let prepared = self.text.prepare_configure(request)?;
        let prepared_tab = self
            .tab_text
            .prepare_configure(tab_font_request(font_size, scale.0)?)?;
        let layout = GridLayout::calculate_with_style(
            logical,
            scale,
            content_insets(self.app.config(), self.current_tabs().len()),
            prepared.metrics(),
            self.app.config().font.line_spacing,
            prepared.generation(),
        )?;
        let grid_changed = self.layout.grid != layout.grid;
        let tab_bar = crate::tab::TabBarPresentation::layout(
            self.current_tabs(),
            layout.viewport_px.width,
            scale.0,
            &self.app.config().tabs,
            self.tab_bar.offset,
        );
        if grid_changed {
            for tab in self.current_tabs_mut().tabs_mut() {
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
        self.visual_build = None;
        self.pending_visual_map = None;
        self.published_visual_map = None;
        self.cancel_pointer_gesture();
        self.layout_generation = self
            .layout_generation
            .checked_add(1)
            .ok_or_else(|| UiRuntimeError::Grid("layout generation overflow".into()))?;
        self.tab_bar = tab_bar;
        self.refresh_text_input_rectangle()?;
        if !grid_changed {
            self.compose_latest()?;
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
            content_insets(self.app.config(), self.current_tabs().len()),
            self.text.metrics(),
            self.app.config().font.line_spacing,
            self.text.generation(),
        )?;
        let grid_changed = self.layout.grid != layout.grid;
        if grid_changed {
            for tab in self.current_tabs_mut().tabs_mut() {
                if let Err(error) = tab.session.resize(layout.grid) {
                    tab.session.mark_failed();
                    tab.runtime.fast_cancel();
                    tracing::warn!(category = "tab_session_failed", session_id = tab.id.get(), %error, "tab resize failed");
                }
            }
        }
        self.layout = layout;
        self.visual_build = None;
        self.pending_visual_map = None;
        self.published_visual_map = None;
        self.cancel_pointer_gesture();
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
        for tab in self.current_tabs_mut().tabs_mut() {
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
                for tab in self.current_tabs_mut().tabs_mut() {
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
        let physical = (key.serial.seat, key.physical_keycode);
        if key.state == leyline_gfx::KeyState::Released {
            if self.local_pressed_keys.remove(&physical) {
                return Ok(());
            }
            if let Some(pressed) = self.terminal_pressed_keys.remove(&physical) {
                return self.send_terminal_keyboard_event(
                    key,
                    pressed,
                    crate::terminal::KeyboardEventKind::Release,
                );
            }
            return Ok(());
        }
        if key.repeat
            && let Some(pressed) = self.terminal_pressed_keys.get(&physical).copied()
        {
            return self.send_terminal_keyboard_event(
                key,
                pressed,
                crate::terminal::KeyboardEventKind::Repeat,
            );
        }
        if self.handle_paste_confirmation_key(key)? {
            self.remember_local_key(physical);
            return Ok(());
        }
        self.last_input_serial = Some(key.serial);
        if should_cancel_search(self.active_session().search().is_open(), key.logical_key) {
            self.execute_action(crate::config::Action::CancelSearch)?;
            self.remember_local_key(physical);
            return Ok(());
        }
        if self.search_focused
            && self.active_session().search().is_open()
            && self.handle_search_key(key)?
        {
            self.remember_local_key(physical);
            return Ok(());
        }
        if let Some(action) = self.resolve_shortcut(key) {
            tracing::debug!(
                ?action,
                logical_key = ?key.logical_key,
                "configurable shortcut matched"
            );
            if !(key.repeat && ignores_key_repeat(action)) {
                self.execute_action(action)?;
            }
            self.remember_local_key(physical);
            return Ok(());
        }
        if key.repeat && self.local_pressed_keys.contains(&physical) {
            return Ok(());
        }
        if self
            .local_pressed_keys
            .len()
            .saturating_add(self.terminal_pressed_keys.len())
            >= MAX_PRESSED_KEYS
        {
            self.local_pressed_keys.clear();
            self.terminal_pressed_keys.clear();
            self.remember_local_key(physical);
            tracing::warn!(
                category = "pressed_key_overflow",
                "pressed-key ownership table reset"
            );
            return Ok(());
        }
        let pressed = PressedTerminalKey {
            owner: self
                .current_tabs_mut()
                .active_id()
                .expect("runtime always has an active tab"),
            identity: key.identity,
            // Text-input focus is normally active for the whole terminal focus lifetime.
            // Only an actual preedit proves that the IME currently owns text composition.
            associated_text_allowed: xkb_text_allowed(&self.ime),
            foreground_process_group: self.active_session().foreground_process_group(),
        };
        self.terminal_pressed_keys.insert(physical, pressed);
        self.send_terminal_keyboard_event(key, pressed, crate::terminal::KeyboardEventKind::Press)
    }

    fn remember_local_key(&mut self, physical: (leyline_gfx::SeatToken, u32)) {
        if !self.local_pressed_keys.contains(&physical)
            && self
                .local_pressed_keys
                .len()
                .saturating_add(self.terminal_pressed_keys.len())
                >= MAX_PRESSED_KEYS
        {
            self.local_pressed_keys.clear();
            self.terminal_pressed_keys.clear();
            tracing::warn!(
                category = "pressed_key_overflow",
                "pressed-key ownership table reset"
            );
        }
        self.local_pressed_keys.insert(physical);
    }

    fn send_terminal_keyboard_event(
        &mut self,
        key: &leyline_gfx::KeyInput,
        pressed: PressedTerminalKey,
        kind: crate::terminal::KeyboardEventKind,
    ) -> Result<(), UiRuntimeError> {
        let modifiers = terminal_modifiers(key);
        let text = if kind == crate::terminal::KeyboardEventKind::Release {
            None
        } else {
            key_text(key)
        };
        let event = crate::terminal::TerminalKeyboardEvent {
            identity: pressed.identity,
            text: text.clone(),
            modifiers,
            caps_lock: key.modifiers.caps_lock,
            num_lock: key.modifiers.num_lock,
            kind,
            associated_text_allowed: pressed.associated_text_allowed,
        };
        let Some(owner) = self.current_tabs_mut().get_mut(pressed.owner) else {
            return Ok(());
        };
        let current_process_group = owner.session.foreground_process_group();
        if kind != crate::terminal::KeyboardEventKind::Press
            && terminal_key_owner_changed(pressed.foreground_process_group, current_process_group)
        {
            tracing::debug!(
                pressed_process_group = pressed.foreground_process_group,
                current_process_group,
                ?kind,
                "discarding keyboard event after foreground job change"
            );
            return Ok(());
        }
        let fallback = owner.session.input_keyboard_event(&event)?;
        if fallback
            && pressed.associated_text_allowed
            && let Some(text) = text
        {
            let Some(owner) = self.current_tabs_mut().get_mut(pressed.owner) else {
                return Ok(());
            };
            owner.session.commit_text(&text)?;
        }
        if starts_terminal_control_gesture(pressed.identity.logical, modifiers, kind) {
            self.terminal_control_gesture = true;
        }
        Ok(())
    }

    fn handle_search_key(&mut self, key: &leyline_gfx::KeyInput) -> Result<bool, UiRuntimeError> {
        use crate::search::{SearchDirection, SearchEdit};
        let modifiers = terminal_modifiers(key);
        let now = Instant::now();
        let edit = match key.logical_key {
            leyline_gfx::LogicalKey::Enter => {
                let direction = if modifiers.shift {
                    SearchDirection::Previous
                } else {
                    SearchDirection::Next
                };
                self.active_session_mut().navigate_search(direction, now)?;
                self.compose_latest()?;
                return Ok(true);
            }
            leyline_gfx::LogicalKey::Backspace => Some(SearchEdit::Backspace),
            leyline_gfx::LogicalKey::Delete => Some(SearchEdit::Delete),
            leyline_gfx::LogicalKey::ArrowLeft => Some(SearchEdit::Left),
            leyline_gfx::LogicalKey::ArrowRight => Some(SearchEdit::Right),
            leyline_gfx::LogicalKey::Home => Some(SearchEdit::Home),
            leyline_gfx::LogicalKey::End => Some(SearchEdit::End),
            _ => None,
        };
        if let Some(edit) = edit {
            self.active_session_mut().edit_search(edit, now);
            self.compose_latest()?;
            self.refresh_text_input_rectangle()?;
            return Ok(true);
        }
        if modifiers.control
            && matches!(
                key.logical_key,
                leyline_gfx::LogicalKey::Character('g' | 'G')
            )
        {
            let direction = if modifiers.shift {
                SearchDirection::Previous
            } else {
                SearchDirection::Next
            };
            self.active_session_mut().navigate_search(direction, now)?;
            self.compose_latest()?;
            return Ok(true);
        }
        if let Some(action) = self.resolve_shortcut(key) {
            match action {
                crate::config::Action::PastePrimary => {}
                crate::config::Action::CopyClipboard
                | crate::config::Action::PasteClipboard
                | crate::config::Action::IncreaseFontSize
                | crate::config::Action::DecreaseFontSize
                | crate::config::Action::ResetFontSize
                | crate::config::Action::ScrollPageUp
                | crate::config::Action::ScrollPageDown
                | crate::config::Action::NewTab
                | crate::config::Action::NewWindow
                | crate::config::Action::CloseTab
                | crate::config::Action::PreviousTab
                | crate::config::Action::NextTab
                | crate::config::Action::MoveTabLeft
                | crate::config::Action::MoveTabRight
                | crate::config::Action::MoveTabToNewWindow
                | crate::config::Action::ToggleFullscreen
                | crate::config::Action::ToggleMaximized
                | crate::config::Action::RestoreWindow
                | crate::config::Action::ActivateTab(_)
                | crate::config::Action::ToggleBellMute
                | crate::config::Action::Search
                | crate::config::Action::SearchNext
                | crate::config::Action::SearchPrevious
                | crate::config::Action::CancelSearch => self.execute_action(action)?,
            }
            return Ok(true);
        }
        if !modifiers.control
            && !modifiers.alt
            && let Some(text) = key_text(key)
        {
            self.active_session_mut()
                .edit_search(SearchEdit::Insert(&text), now);
            self.compose_latest()?;
            self.refresh_text_input_rectangle()?;
        }
        Ok(true)
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
                self.ime_context = None;
                self.enable_text_input()?;
            }
            TextInputEvent::Leave => {
                self.ime.deactivate();
                self.ime_context = None;
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
                let search_focused =
                    self.search_focused && self.active_session().search().is_open();
                if let Some((before_bytes, after_bytes)) = done.delete_surrounding {
                    if search_focused {
                        self.active_session_mut().edit_search(
                            crate::search::SearchEdit::DeleteSurrounding {
                                before_bytes,
                                after_bytes,
                            },
                            Instant::now(),
                        );
                    } else {
                        tracing::warn!("IME delete-surrounding request ignored for terminal input");
                    }
                }
                if let Some(commit) = done.commit {
                    let text = std::str::from_utf8(&commit)
                        .map_err(|_| crate::interaction::ImeError::CommitTooLarge)?;
                    if search_focused {
                        self.active_session_mut()
                            .edit_search(crate::search::SearchEdit::Insert(text), Instant::now());
                    } else {
                        self.active_session_mut().commit_text(text)?;
                    }
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
        let Some(context) = self.text_input_context() else {
            return Ok(());
        };
        if self.ime_context.as_ref() == Some(&context) && !self.ime.outbound.dirty {
            return Ok(());
        }
        let serial = self.gfx.update_text_input(context.clone())?;
        if let Some(serial) = serial {
            self.ime.record_commit_serial(serial);
            self.ime_context = Some(context);
        }
        Ok(())
    }

    fn enable_text_input(&mut self) -> Result<(), UiRuntimeError> {
        if !self.ime.is_active() || !self.gfx.text_input_available() {
            return Ok(());
        }
        let Some(context) = self.text_input_context() else {
            return Ok(());
        };
        if let Some(serial) = self.gfx.enable_text_input(context.clone())? {
            self.ime.record_commit_serial(serial);
            self.ime_context = Some(context);
        }
        Ok(())
    }

    fn text_input_rectangle(&self) -> Option<leyline_gfx::TextInputRectangle> {
        let scale = self.gfx.scale().0.max(1);
        let logical = |value: u32| {
            i32::try_from(u64::from(value) * 120 / u64::from(scale)).unwrap_or(i32::MAX)
        };
        if self.search_focused && self.active_session().search().is_open() {
            let dialog = self.search_dialog.clone().unwrap_or_else(|| {
                let viewport = [
                    self.layout.viewport_px.width,
                    self.layout.viewport_px.height,
                ];
                let mut dialog = crate::search::SearchDialogPresentation::layout(
                    viewport,
                    self.gfx.scale().0,
                    self.active_session().search().query().to_owned(),
                );
                if let Some(origin) = self.search_dialog_origin {
                    dialog.move_to(origin, viewport);
                }
                dialog
            });
            let input = dialog.input;
            return Some(leyline_gfx::TextInputRectangle {
                x: logical(input.x),
                y: logical(input.y),
                width: logical(input.width).max(1),
                height: logical(input.height).max(1),
            });
        }
        let snapshot = self.active_session().latest_snapshot()?;
        let visual_column = self.published_visual_map.as_ref().map_or_else(
            || (!self.app.config().unicode.bidi).then_some(snapshot.cursor.column),
            |map| {
                (map.snapshot_generation == snapshot.generation)
                    .then(|| {
                        map.lines
                            .get(usize::from(snapshot.cursor.line))?
                            .logical_to_visual_cell
                            .get(usize::from(snapshot.cursor.column))
                            .copied()
                    })
                    .flatten()
            },
        )?;
        let physical_x = self.layout.content_origin_px[0]
            .saturating_add(u32::from(visual_column) * u32::from(self.layout.cell_px[0].get()));
        let physical_y = self.layout.content_origin_px[1].saturating_add(
            u32::from(snapshot.cursor.line) * u32::from(self.layout.cell_px[1].get()),
        );
        Some(leyline_gfx::TextInputRectangle {
            x: logical(physical_x),
            y: logical(physical_y),
            width: logical(u32::from(self.layout.cell_px[0].get())).max(1),
            height: logical(u32::from(self.layout.cell_px[1].get())).max(1),
        })
    }

    fn text_input_context(&self) -> Option<leyline_gfx::TextInputContext> {
        let rectangle = self.text_input_rectangle()?;
        if self.search_focused && self.active_session().search().is_open() {
            let search = self.active_session().search();
            leyline_gfx::TextInputContext::search(
                rectangle,
                search.query().to_owned(),
                search.cursor_byte(),
            )
            .ok()
        } else {
            Some(leyline_gfx::TextInputContext::terminal(rectangle))
        }
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
        let builder = begin_visual_map(
            snapshot,
            UnicodePolicy {
                bidi: self.app.config().unicode.bidi,
                generation: self.layout_generation,
            },
        )?;
        self.visual_build = Some(PendingVisualBuild {
            snapshot: snapshot.clone(),
            builder,
        });
        Ok(())
    }

    fn advance_visual_build(&mut self) -> Result<(), UiRuntimeError> {
        let Some(mut pending) = self.visual_build.take() else {
            return Ok(());
        };
        match pending
            .builder
            .step(Instant::now() + Duration::from_millis(2))?
        {
            BuildStep::Pending => self.visual_build = Some(pending),
            BuildStep::Ready(map) => {
                self.compose_with_visual_map(&pending.snapshot, Arc::new(map))?;
            }
        }
        Ok(())
    }

    fn compose_with_visual_map(
        &mut self,
        snapshot: &crate::terminal::FrameSnapshot,
        visual_map: Arc<VisualGridMap>,
    ) -> Result<(), UiRuntimeError> {
        self.update_tab_bar();
        let search_dialog = self.search_dialog_presentation();
        self.search_dialog.clone_from(&search_dialog);
        self.ime.reanchor_preedit(
            snapshot.generation,
            [snapshot.cursor.column, snapshot.cursor.line],
        );
        self.refresh_text_input_rectangle()?;
        let paste_confirmation = self.paste_confirmation_overlay().copied();
        let selection = self.active_session().selection_overlay(snapshot.generation);
        let search = self.active_session().search_overlay(snapshot);
        let search_focused = self.search_focused && self.active_session().search().is_open();
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
        let visual_bell_active = self
            .current_tabs_mut()
            .active_id()
            .is_some_and(|id| self.visual_bell.active_for(id, Instant::now()));
        let colors = self.app.config().colors.clone();
        let current = self.current_window_id;
        let Some(WindowRecord::Ready(window)) = self.windows.get_mut(&current) else {
            return Err(UiRuntimeError::Grid("current window is not ready".into()));
        };
        let mut scene = compose(
            &mut window.text,
            &mut window.tab_text,
            snapshot,
            FrameOverlays {
                selection: &selection,
                search: search.as_ref(),
                preedit: (!search_focused)
                    .then_some(window.ime.preedit.as_ref())
                    .flatten(),
                paste_confirmation: paste_confirmation.as_ref(),
                scrollbar: scrollbar.as_ref(),
                tab_bar: Some(&window.tab_bar),
                search_dialog: search_dialog.as_ref(),
                visual_bell: Some(&crate::bell::VisualBellPresentation {
                    active: visual_bell_active,
                    color: 0xff5a_3628,
                    intensity: 1.0,
                }),
            },
            &window.layout,
            &colors,
            crate::frame_composer::CursorPresentationPolicy {
                blink_phase_visible: window.cursor_blink_visible,
            },
            &visual_map,
            window.layout_generation,
        )?;
        if !self.gfx.scene_fits_atlas(&scene)? {
            if !downgrade_color_working_set(&mut scene) {
                return Err(crate::frame_composer::ComposeError::Capacity("glyph atlas").into());
            }
            tracing::warn!(
                category = "capacity_pressure",
                operation = "color_to_gray_rebuild",
                "mixed atlas exceeded four pages; rebuilt the frame with grayscale emoji"
            );
        }
        let key = scene.frame_key;
        self.gfx.apply(leyline_gfx::GfxCommand::SetScene(scene))?;
        self.pending_visual_map = Some((key, visual_map));
        Ok(())
    }

    fn update_tab_bar(&mut self) {
        self.tab_bar = crate::tab::TabBarPresentation::layout(
            self.current_tabs(),
            self.layout.viewport_px.width,
            self.gfx.scale().0,
            &self.app.config().tabs,
            self.tab_bar.offset,
        );
    }

    fn search_dialog_presentation(&self) -> Option<crate::search::SearchDialogPresentation> {
        let search = self.active_session().search();
        if !search.is_open() {
            return None;
        }
        let mut query = search.query().to_owned();
        let mut cursor = search.cursor_byte();
        if self.search_focused
            && let Some(preedit) = &self.ime.preedit
            && query.is_char_boundary(cursor)
        {
            query.insert_str(cursor, &preedit.text);
            cursor = cursor.saturating_add(preedit.text.len());
        }
        if query.is_char_boundary(cursor) {
            let viewport = [
                self.layout.viewport_px.width,
                self.layout.viewport_px.height,
            ];
            let mut layout = crate::search::SearchDialogPresentation::layout(
                viewport,
                self.gfx.scale().0,
                String::new(),
            );
            if let Some(origin) = self.search_dialog_origin {
                layout.move_to(origin, viewport);
            }
            let visible_chars =
                search_query_capacity(layout.input.width, self.text.metrics().width_px.get());
            layout.query_text =
                visible_search_query(&query, cursor, visible_chars, self.search_focused);
            return Some(layout);
        }
        None
    }

    fn set_search_focus(&mut self, focused: bool) -> Result<(), UiRuntimeError> {
        if self.search_focused == focused || !self.active_session().search().is_open() {
            return Ok(());
        }
        self.search_focused = focused;
        if !focused {
            self.pending_search_paste = None;
        }
        if self.ime.is_active() {
            self.ime.deactivate();
            self.ime.activate();
            self.ime_context = None;
        }
        self.compose_latest()?;
        self.refresh_text_input_rectangle()
    }

    fn set_pointer_cursor(
        &mut self,
        cursor: leyline_gfx::PointerCursor,
    ) -> Result<(), UiRuntimeError> {
        self.gfx
            .apply(leyline_gfx::GfxCommand::SetPointerCursor(cursor))?;
        Ok(())
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
            self.ime_context = None;
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
            .current_tabs_mut()
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

    #[allow(clippy::too_many_lines)]
    fn execute_action(&mut self, action: crate::config::Action) -> Result<(), UiRuntimeError> {
        use crate::config::Action;
        match action {
            Action::CopyClipboard => {
                self.copy_selection(leyline_gfx::SelectionTarget::Clipboard)?;
            }
            Action::PasteClipboard => {
                if self.search_focused && self.active_session().search().is_open() {
                    self.pending_search_paste = Some((
                        self.current_tabs().active_id().expect("active tab"),
                        self.active_session().search().revision(),
                    ));
                } else {
                    self.pending_search_paste = None;
                }
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
            Action::NewWindow => self.new_window()?,
            Action::MoveTabToNewWindow => self.move_active_tab_to_new_window(),
            Action::CloseTab => self.close_active_tab(ShutdownReason::UserRequested)?,
            Action::PreviousTab => self.switch_relative(-1)?,
            Action::NextTab => self.switch_relative(1)?,
            Action::MoveTabLeft | Action::MoveTabRight => {
                let delta = if action == Action::MoveTabLeft { -1 } else { 1 };
                if let crate::tab::ReorderOutcome::Changed { from, to } =
                    self.current_tabs_mut().move_active(delta)?
                {
                    tracing::info!(
                        category = "tab_reorder_committed",
                        from,
                        to,
                        "tab reordered"
                    );
                    self.update_tab_bar();
                    self.compose_latest()?;
                }
            }
            Action::ToggleFullscreen => {
                let target = !self.window_state.desired().fullscreen;
                self.window_state.request(
                    crate::window::WindowStateRequest::SetFullscreen(target),
                    Instant::now(),
                    WINDOW_STATE_STABILIZATION,
                )?;
                self.gfx
                    .apply(leyline_gfx::GfxCommand::RequestFullscreen(target))?;
            }
            Action::ToggleMaximized => {
                let target = !self.window_state.desired().maximized;
                self.window_state.request(
                    crate::window::WindowStateRequest::SetMaximized(target),
                    Instant::now(),
                    WINDOW_STATE_STABILIZATION,
                )?;
                self.gfx
                    .apply(leyline_gfx::GfxCommand::RequestMaximized(target))?;
            }
            Action::RestoreWindow => {
                self.window_state.request(
                    crate::window::WindowStateRequest::Restore,
                    Instant::now(),
                    WINDOW_STATE_STABILIZATION,
                )?;
                self.gfx.apply(leyline_gfx::GfxCommand::RequestRestore)?;
            }
            Action::ActivateTab(ordinal) => self.switch_ordinal(ordinal)?,
            Action::ToggleBellMute => {
                let id = self.current_tabs().active_id().expect("active tab");
                let (muted, invalidated) = self.current_tabs_mut().toggle_bell_mute(id)?;
                if muted {
                    self.visual_bell.cancel();
                }
                if let Some(generation) = invalidated {
                    self.notifications.acknowledge(id, generation);
                }
                tracing::info!(
                    category = "bell_effect",
                    session_id = id.get(),
                    muted,
                    "tab bell mute changed"
                );
                self.compose_latest()?;
            }
            Action::Search => {
                self.active_session_mut().open_search();
                if self.search_focused {
                    self.compose_latest()?;
                    self.refresh_text_input_rectangle()?;
                } else {
                    self.set_search_focus(true)?;
                }
            }
            Action::SearchNext => {
                self.active_session_mut()
                    .navigate_search(crate::search::SearchDirection::Next, Instant::now())?;
                self.compose_latest()?;
            }
            Action::SearchPrevious => {
                self.active_session_mut()
                    .navigate_search(crate::search::SearchDirection::Previous, Instant::now())?;
                self.compose_latest()?;
            }
            Action::CancelSearch => {
                self.pending_search_paste = None;
                self.search_dialog_drag = None;
                self.search_dialog_origin = None;
                self.set_pointer_cursor(leyline_gfx::PointerCursor::Text)?;
                self.set_search_focus(false)?;
                self.active_session_mut().cancel_search();
                self.compose_latest()?;
                self.refresh_text_input_rectangle()?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn new_tab(&mut self) -> Result<(), UiRuntimeError> {
        if !matches!(self.app.lifecycle(), crate::app::Lifecycle::Running) {
            return Ok(());
        }
        if !self.current_tabs().has_capacity() {
            tracing::warn!(
                category = "tab_create_failed",
                limit = self.app.config().tabs.max_count,
                "tab limit reached"
            );
            return Ok(());
        }
        if self.process_session_count() >= MAX_PROCESS_SESSIONS {
            tracing::warn!(
                category = "tab_create_failed",
                limit = MAX_PROCESS_SESSIONS,
                "process session limit reached"
            );
            return Ok(());
        }
        let source_id = self.current_tabs().active_id();
        let (primary, origin) = select_new_tab_cwd(
            &self.app.config().tabs.new_tab_cwd,
            self.current_tabs().active(),
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
        let tab_bar_was_visible = tab_bar_visible(self.app.config(), self.current_tabs().len());
        self.quiesce_active_interaction()?;
        let id = self.session_ids.allocate()?;
        let title = launch_title(self.app.launch());
        match self
            .current_tabs_mut()
            .push_with_id(id, session, runtime, title)
        {
            Ok(id) => {
                self.sessions.mark_active(id, self.current_window_id);
                tracing::info!(
                    category = "tab_created",
                    session_id = id.get(),
                    cwd_origin = final_origin,
                    "tab created"
                );
            }
            Err(error) => {
                tracing::warn!(category = "tab_create_failed", %error, "could not create tab");
            }
        }
        if tab_bar_was_visible != tab_bar_visible(self.app.config(), self.current_tabs().len()) {
            self.reconfigure_layout(self.gfx.logical_size(), self.gfx.scale(), self.font_size)?;
        }
        self.restore_active_interaction()?;
        let snapshot = self.active_session_mut().end_drain_round()?;
        if let Some(snapshot) = snapshot {
            self.compose_snapshot(&snapshot)?;
        }
        self.refresh_active_title()
    }

    fn new_window(&mut self) -> Result<(), UiRuntimeError> {
        if !matches!(self.app.lifecycle(), crate::app::Lifecycle::Running) {
            return Ok(());
        }
        if self.process_session_count() >= MAX_PROCESS_SESSIONS {
            tracing::warn!(
                category = "window_create_failed",
                limit = MAX_PROCESS_SESSIONS,
                reason = "process_session_limit",
                "new window request failed"
            );
            return Ok(());
        }
        let (candidate, _) = select_new_tab_cwd(
            &self.app.config().tabs.new_tab_cwd,
            self.current_tabs().active(),
            self.app.launch_context(),
        );
        let cwd = SpawnDirectory::open(&candidate)
            .or_else(|_| SpawnDirectory::open(&self.app.launch_context().base_cwd))?;
        let request = self.gfx.create_window(&GfxOptions {
            title: "Leyline".into(),
            default_size: self.gfx.logical_size(),
            clear: LinearColor::from_srgba8(self.app.config().colors.background.0),
        });
        let id = match request {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(
                    category = "window_create_failed",
                    %error,
                    "new window request failed"
                );
                return Ok(());
            }
        };
        self.windows.insert(id, WindowRecord::Creating { cwd });
        tracing::info!(
            category = "window_create_requested",
            current_window_id = id.get(),
            "new window requested"
        );
        Ok(())
    }

    fn move_active_tab_to_new_window(&mut self) {
        let Some(session) = self.current_tabs().active_id() else {
            return;
        };
        let request = self.gfx.create_window(&GfxOptions {
            title: "Leyline".into(),
            default_size: self.gfx.logical_size(),
            clear: LinearColor::from_srgba8(self.app.config().colors.background.0),
        });
        let id = match request {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(
                    category = "tab_move_rolled_back",
                    session_id = session.get(),
                    %error,
                    "tab move target request failed"
                );
                return;
            }
        };
        self.windows.insert(
            id,
            WindowRecord::Moving {
                source: self.current_window_id,
                session,
            },
        );
        tracing::info!(
            category = "tab_move_prepared",
            current_window_id = id.get(),
            session_id = session.get(),
            "tab move target requested"
        );
    }

    fn handle_pending_window_event(
        &mut self,
        id: leyline_gfx::WindowId,
        event: &PlatformEvent,
    ) -> Result<(), UiRuntimeError> {
        match event {
            PlatformEvent::Configured { .. }
                if matches!(self.windows.get(&id), Some(WindowRecord::Creating { .. })) =>
            {
                self.finish_window_creation(id)?;
            }
            PlatformEvent::Configured { .. }
                if matches!(self.windows.get(&id), Some(WindowRecord::Moving { .. })) =>
            {
                self.finish_tab_move(id)?;
            }
            PlatformEvent::CloseRequested => {
                if let Some(WindowRecord::Moving { session, .. }) = self.windows.remove(&id) {
                    tracing::info!(
                        category = "tab_move_rolled_back",
                        current_window_id = id.get(),
                        session_id = session.get(),
                        reason = "target_closed",
                        "tab move target closed before commit"
                    );
                }
                self.gfx.remove_window(id)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn finish_window_creation(&mut self, id: leyline_gfx::WindowId) -> Result<(), UiRuntimeError> {
        let Some(WindowRecord::Creating { cwd }) = self.windows.remove(&id) else {
            return Ok(());
        };
        if self.gfx.window(id).is_none() {
            self.windows.insert(id, WindowRecord::Creating { cwd });
            return Ok(());
        }
        let prepared = (|| {
            let presentation = self.prepare_window_presentation(id)?;
            let session_id = self.session_ids.allocate()?;
            let runtime = AppRuntimeBuilder::new(self.wake_backend.clone()).build()?;
            let session = TerminalSession::start(
                self.app.launch(),
                cwd,
                self.app.config(),
                presentation.layout.grid,
                &runtime,
            )?;
            Ok::<_, UiRuntimeError>((presentation, session_id, runtime, session))
        })();
        let (presentation, session_id, runtime, session) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.gfx.remove_window(id)?;
                tracing::warn!(
                    category = "window_create_failed",
                    current_window_id = id.get(),
                    reason = ?error.category(),
                    "new window preparation failed"
                );
                return Ok(());
            }
        };
        let scale = self
            .gfx
            .window(id)
            .expect("configured window has graphics state")
            .scale();
        let font_size = self.app.config().font.size;
        self.sessions.insert(
            id,
            TabManager::bootstrap_with_id(
                session_id,
                session,
                runtime,
                window_tab_limit(self.app.config()),
            ),
        );
        self.windows.insert(
            id,
            WindowRecord::Ready(Box::new(WindowRuntime::ready(
                presentation.text,
                presentation.tab_text,
                presentation.layout,
                scale,
                font_size,
                crate::window::DesiredWindowState::default(),
            ))),
        );
        self.reconfigure_window_layout(id)?;
        tracing::info!(
            category = "window_create_committed",
            current_window_id = id.get(),
            session_id = session_id.get(),
            "new window ready"
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn finish_tab_move(&mut self, id: leyline_gfx::WindowId) -> Result<(), UiRuntimeError> {
        let Some(WindowRecord::Moving { source, session }) = self.windows.remove(&id) else {
            return Ok(());
        };
        let source_exists = self
            .sessions
            .get(source)
            .is_some_and(|tabs| tabs.tabs().iter().any(|tab| tab.id == session));
        if !source_exists {
            self.gfx.remove_window(id)?;
            tracing::info!(
                category = "tab_move_rolled_back",
                current_window_id = id.get(),
                session_id = session.get(),
                reason = "source_changed",
                "tab move rolled back"
            );
            return Ok(());
        }
        if self.gfx.window(id).is_none() {
            self.windows
                .insert(id, WindowRecord::Moving { source, session });
            return Ok(());
        }
        let presentation = match self.prepare_window_presentation(id) {
            Ok(presentation) => presentation,
            Err(error) => {
                self.gfx.remove_window(id)?;
                tracing::warn!(
                    category = "tab_move_rolled_back",
                    current_window_id = id.get(),
                    session_id = session.get(),
                    reason = ?error.category(),
                    "tab move target preparation failed"
                );
                return Ok(());
            }
        };

        if !self.activate_window(source) {
            self.gfx.remove_window(id)?;
            return Ok(());
        }
        self.quiesce_active_interaction()?;
        let scale = self
            .gfx
            .window(id)
            .expect("configured window has graphics state")
            .scale();
        let font_size = self.app.config().font.size;
        let (from, source_empty) = self.sessions.commit_move_to_new_window(
            source,
            id,
            session,
            window_tab_limit(self.app.config()),
        )?;
        self.windows.insert(
            id,
            WindowRecord::Ready(Box::new(WindowRuntime::ready(
                presentation.text,
                presentation.tab_text,
                presentation.layout,
                scale,
                font_size,
                crate::window::DesiredWindowState::default(),
            ))),
        );
        self.activate_window(id);
        let target_tab = self
            .sessions
            .get_mut(id)
            .and_then(TabManager::active_mut)
            .expect("committed move target owns the moved session");
        if let Err(error) = target_tab.session.focus_changed(false) {
            target_tab.session.mark_failed();
            target_tab.runtime.fast_cancel();
            tracing::warn!(
                category = "tab_session_failed",
                session_id = session.get(),
                %error,
                "moved tab focus transition failed"
            );
        }
        if let Err(error) = target_tab.session.resize(presentation.layout.grid) {
            target_tab.session.mark_failed();
            target_tab.runtime.fast_cancel();
            tracing::warn!(
                category = "tab_session_failed",
                session_id = session.get(),
                %error,
                "moved tab resize failed"
            );
        }
        self.reconfigure_window_layout(id)?;
        if source_empty {
            self.close_window_window(source)?;
        } else {
            self.activate_window(source);
            self.reconfigure_window_layout(source)?;
            self.restore_active_interaction()?;
            self.refresh_active_title()?;
            self.activate_window(id);
        }
        let source_current_window_id = source.get();
        tracing::info!(
            category = "tab_move_committed",
            source_current_window_id,
            target_current_window_id = id.get(),
            session_id = session.get(),
            from,
            "tab moved without restarting PTY"
        );
        Ok(())
    }

    fn prepare_window_presentation(
        &self,
        id: leyline_gfx::WindowId,
    ) -> Result<WindowPresentation, UiRuntimeError> {
        let gfx = self
            .gfx
            .window(id)
            .ok_or_else(|| UiRuntimeError::Grid("window window is not configured".into()))?;
        let logical = gfx.logical_size();
        let scale = gfx.scale();
        let request = FontRequest::from_points(
            self.app.config().font.family.clone(),
            self.app.config().font.size,
            scale.0,
            self.app.config().font.ligatures,
        )?
        .with_rendering(
            text_hinting(self.app.config().font.hinting),
            text_antialiasing(self.app.config().font.antialiasing),
        )
        .with_color_glyphs(self.app.config().unicode.color_glyphs && gfx.color_glyphs_supported());
        let text = TextSystem::new(request)?;
        let tab_text = TextSystem::new(tab_font_request(self.app.config().font.size, scale.0)?)?;
        let layout = GridLayout::calculate_with_style(
            logical,
            scale,
            content_insets(self.app.config(), 1),
            text.metrics(),
            self.app.config().font.line_spacing,
            text.generation(),
        )?;
        Ok(WindowPresentation {
            text,
            tab_text,
            layout,
        })
    }

    fn reconfigure_window_layout(
        &mut self,
        id: leyline_gfx::WindowId,
    ) -> Result<(), UiRuntimeError> {
        let Some(gfx) = self.gfx.window(id) else {
            return Ok(());
        };
        let (logical, scale) = (gfx.logical_size(), gfx.scale());
        let Some(tabs) = self.sessions.get_mut(id) else {
            return Ok(());
        };
        let Some(WindowRecord::Ready(window)) = self.windows.get_mut(&id) else {
            return Ok(());
        };
        let layout = GridLayout::calculate_with_style(
            logical,
            scale,
            content_insets(self.app.config(), tabs.len()),
            window.text.metrics(),
            self.app.config().font.line_spacing,
            window.text.generation(),
        )?;
        if layout.grid != window.layout.grid {
            for tab in tabs.tabs_mut() {
                tab.session.resize(layout.grid)?;
            }
        }
        window.tab_bar = crate::tab::TabBarPresentation::layout(
            tabs,
            layout.viewport_px.width,
            scale.0,
            &self.app.config().tabs,
            window.tab_bar.offset,
        );
        window.layout = layout;
        window.layout_generation = window.layout_generation.saturating_add(1);
        Ok(())
    }

    fn process_session_count(&self) -> usize {
        self.sessions
            .windows
            .values()
            .map(TabManager::total_len)
            .sum::<usize>()
            .saturating_add(
                self.windows
                    .values()
                    .filter(|window| matches!(window, WindowRecord::Creating { .. }))
                    .count(),
            )
    }

    fn close_window_window(&mut self, id: leyline_gfx::WindowId) -> Result<(), UiRuntimeError> {
        let Some(window) = self.windows.remove(&id) else {
            return Ok(());
        };
        self.gfx.remove_window(id)?;
        let session_id = match &window {
            WindowRecord::Ready(_) => self
                .sessions
                .get(id)
                .and_then(TabManager::active_id)
                .map(crate::tab::SessionId::get),
            WindowRecord::Creating { .. } | WindowRecord::Moving { .. } | WindowRecord::Closing => {
                None
            }
        };
        if matches!(window, WindowRecord::Ready(_)) {
            let mut closing = Vec::new();
            if let Some(tabs) = self.sessions.get_mut(id) {
                while let Some(session) = tabs.close_active() {
                    closing.push(session);
                }
            }
            for session in closing {
                self.sessions.mark_closing(session, id);
            }
            self.windows.insert(id, WindowRecord::Closing);
        }
        tracing::info!(
            category = "window_close_started",
            current_window_id = id.get(),
            session_id,
            "window window closing"
        );
        Ok(())
    }

    fn close_current_window(&mut self) -> Result<(), UiRuntimeError> {
        let closing = self.current_window_id;
        let next = self.windows.iter().find_map(|(id, window)| {
            (*id != closing && matches!(window, WindowRecord::Ready(_))).then_some(*id)
        });
        let Some(next) = next else {
            self.handle_app_event(crate::app::event::AppEvent::ShutdownRequested(
                ShutdownReason::UserRequested,
            ))?;
            return Ok(());
        };
        self.activate_window(next);
        self.close_window_window(closing)
    }

    fn service_background_windows(
        &mut self,
        process_deadline: Instant,
    ) -> Result<bool, UiRuntimeError> {
        let original = self.current_window_id;
        let mut ids = self
            .windows
            .iter()
            .filter_map(|(id, window)| {
                (*id != original && matches!(window, WindowRecord::Ready(_))).then_some(*id)
            })
            .collect::<Vec<_>>();
        rotate_window_service_order(&mut ids, self.window_service_cursor);
        for id in ids {
            if Instant::now() >= process_deadline {
                self.activate_window(original);
                return Ok(true);
            }
            if !self.activate_window(id) {
                continue;
            }
            self.window_service_cursor = Some(id);
            self.apply_settled_resize()?;
            self.flush_expired_sync(Instant::now())?;
            self.drain_sessions(process_deadline)?;
            if self.current_tabs().is_empty() {
                self.close_current_window()?;
                continue;
            }
            self.process_drag_scroll()?;
            self.process_tab_drag_scroll()?;
            if self.scrollbar.expire(Instant::now()) || self.visual_bell.expire(Instant::now()) {
                self.compose_latest()?;
            }
            let search_effect = self.active_session_mut().advance_search(Instant::now())?;
            if search_effect.needs_frame && search_effect.scroll_target.is_none() {
                self.compose_latest()?;
            }
            if let Some(snapshot) = self.active_session_mut().end_drain_round()? {
                self.compose_snapshot(&snapshot)?;
            }
            self.advance_visual_build()?;
            self.advance_cursor_blink(Instant::now())?;
            self.refresh_active_title()?;
            let resize_preview = self.resize_settle_deadline.is_some();
            let _ = self.try_render_current_isolated(resize_preview)?;
        }
        let closing = self
            .windows
            .iter()
            .filter_map(|(id, window)| matches!(window, WindowRecord::Closing).then_some(*id))
            .collect::<Vec<_>>();
        for id in closing {
            let finished = match self.windows.get(&id) {
                Some(WindowRecord::Closing) => {
                    let tabs = self
                        .sessions
                        .get_mut(id)
                        .expect("closing sessions registered");
                    tabs.poll_closing(Instant::now())?;
                    tabs.closing_is_empty()
                }
                _ => false,
            };
            self.sessions.reconcile(id);
            if finished {
                self.windows.remove(&id);
                self.sessions.remove(id);
                tracing::info!(
                    category = "window_close_completed",
                    current_window_id = id.get(),
                    "window closed"
                );
            }
        }
        self.activate_window(original);
        Ok(false)
    }

    fn close_active_tab(&mut self, reason: ShutdownReason) -> Result<(), UiRuntimeError> {
        let tab_bar_was_visible = tab_bar_visible(self.app.config(), self.current_tabs().len());
        self.quiesce_active_interaction()?;
        self.active_session_mut().finish_io_round()?;
        let closed = self.current_tabs_mut().close_active();
        if let Some(id) = closed {
            self.sessions.mark_closing(id, self.current_window_id);
            self.notifications.forget(id);
            self.sound.forget(id);
            tracing::info!(
                category = "tab_close_requested",
                session_id = id.get(),
                "tab closing"
            );
        }
        if self.current_tabs().is_empty() {
            if self.windows.iter().any(|(id, window)| {
                *id != self.current_window_id && matches!(window, WindowRecord::Ready(_))
            }) {
                self.close_current_window()?;
            } else {
                self.handle_app_event(crate::app::event::AppEvent::ShutdownRequested(reason))?;
            }
            return Ok(());
        }
        if tab_bar_was_visible != tab_bar_visible(self.app.config(), self.current_tabs().len()) {
            self.reconfigure_layout(self.gfx.logical_size(), self.gfx.scale(), self.font_size)?;
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
            self.current_tabs_mut().activate_relative(delta),
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
            self.current_tabs_mut().activate_ordinal(ordinal),
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
            self.current_tabs_mut().activate(id),
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
        self.visual_build = None;
        self.pending_visual_map = None;
        self.published_visual_map = None;
        self.visual_bell.cancel();
        if let Some(serial) = self.gfx.disable_text_input()? {
            self.ime.record_commit_serial(serial);
        }
        self.ime.deactivate();
        self.ime_context = None;
        if self.keyboard_focused {
            self.active_session_mut().focus_changed(false)?;
        }
        Ok(())
    }

    fn restore_active_interaction(&mut self) -> Result<(), UiRuntimeError> {
        if self.keyboard_focused {
            if let Some(id) = self.current_tabs().active_id()
                && let Some(generation) = self.current_tabs_mut().acknowledge(id)
            {
                self.notifications.acknowledge(id, generation);
            }
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
        if matches!(event.kind, leyline_gfx::PointerKind::Leave { .. })
            && self.tab_drag.take().is_some()
        {
            self.tab_drag_scroll = None;
            self.set_pointer_cursor(leyline_gfx::PointerCursor::Text)?;
            self.compose_latest()?;
            return Ok(());
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
        let viewport = [
            self.layout.viewport_px.width,
            self.layout.viewport_px.height,
        ];
        let drag_outset = 8_u32.saturating_mul(self.gfx.scale().0).saturating_add(119) / 120;
        let drag_hit = self.search_dialog.as_ref().is_some_and(|dialog| {
            self.active_session().search().is_open()
                && dialog.drag_hit_test(pixel, viewport, drag_outset)
        });
        if let Some(drag) = self.search_dialog_drag {
            self.gfx.apply(leyline_gfx::GfxCommand::SetPointerCursor(
                leyline_gfx::PointerCursor::Grabbing,
            ))?;
            match event.kind {
                leyline_gfx::PointerKind::Motion { .. } => {
                    self.search_dialog_origin = Some([
                        u32::try_from(
                            (i64::from(pixel[0]) - drag.grab_offset[0])
                                .clamp(0, i64::from(u32::MAX)),
                        )
                        .unwrap_or(0),
                        u32::try_from(
                            (i64::from(pixel[1]) - drag.grab_offset[1])
                                .clamp(0, i64::from(u32::MAX)),
                        )
                        .unwrap_or(0),
                    ]);
                    self.compose_latest()?;
                    return Ok(());
                }
                leyline_gfx::PointerKind::Release { button: 0x110, .. } => {
                    self.search_dialog_drag = None;
                    self.gfx
                        .apply(leyline_gfx::GfxCommand::SetPointerCursor(if drag_hit {
                            leyline_gfx::PointerCursor::Grab
                        } else {
                            leyline_gfx::PointerCursor::Text
                        }))?;
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        self.gfx
            .apply(leyline_gfx::GfxCommand::SetPointerCursor(if drag_hit {
                leyline_gfx::PointerCursor::Grab
            } else {
                leyline_gfx::PointerCursor::Text
            }))?;
        if self.active_session().search().is_open()
            && let Some(dialog) = self.search_dialog.clone()
        {
            let primary_press = matches!(
                event.kind,
                leyline_gfx::PointerKind::Press { button: 0x110, .. }
            );
            if dialog.panel.contains(pixel) {
                if primary_press && dialog.input.contains(pixel) {
                    self.set_search_focus(true)?;
                } else if primary_press && dialog.previous.contains(pixel) {
                    self.active_session_mut().navigate_search(
                        crate::search::SearchDirection::Previous,
                        Instant::now(),
                    )?;
                    self.compose_latest()?;
                } else if primary_press && dialog.next.contains(pixel) {
                    self.active_session_mut()
                        .navigate_search(crate::search::SearchDirection::Next, Instant::now())?;
                    self.compose_latest()?;
                } else if primary_press {
                    self.search_dialog_drag = Some(SearchDialogDrag {
                        grab_offset: [
                            i64::from(pixel[0]) - i64::from(dialog.panel.x),
                            i64::from(pixel[1]) - i64::from(dialog.panel.y),
                        ],
                    });
                    self.gfx.apply(leyline_gfx::GfxCommand::SetPointerCursor(
                        leyline_gfx::PointerCursor::Grabbing,
                    ))?;
                }
                return Ok(());
            }
            if primary_press && drag_hit {
                self.search_dialog_drag = Some(SearchDialogDrag {
                    grab_offset: [
                        i64::from(pixel[0]) - i64::from(dialog.panel.x),
                        i64::from(pixel[1]) - i64::from(dialog.panel.y),
                    ],
                });
                self.gfx.apply(leyline_gfx::GfxCommand::SetPointerCursor(
                    leyline_gfx::PointerCursor::Grabbing,
                ))?;
                return Ok(());
            }
            if primary_press {
                self.set_search_focus(false)?;
            }
        }
        if let Some(mut drag) = self.tab_drag.take() {
            match event.kind {
                leyline_gfx::PointerKind::Motion { .. } => {
                    drag.current = [f64::from(pixel[0]), f64::from(pixel[1])];
                    let dx = drag.current[0] - drag.origin[0];
                    let dy = drag.current[1] - drag.origin[1];
                    let threshold = 8.0 * scale;
                    if dx.mul_add(dx, dy * dy) >= threshold * threshold {
                        drag.phase = crate::tab::TabDragPhase::Dragging;
                    }
                    if drag.phase == crate::tab::TabDragPhase::Dragging {
                        if let Some(index) =
                            self.tab_bar.proposed_index(self.current_tabs(), pixel[0])
                        {
                            drag.proposed_index = index;
                        }
                        let edge_direction = self
                            .tab_bar
                            .bar
                            .and_then(|bar| {
                                tab_drag_edge_direction(bar, pixel[0], self.gfx.scale().0)
                            })
                            .filter(|_| self.tab_bar.max_offset > 0);
                        self.tab_drag_scroll = edge_direction.map(|direction| {
                            self.tab_drag_scroll
                                .filter(|scroll| scroll.direction == direction)
                                .unwrap_or(TabDragScroll {
                                    direction,
                                    deadline: Instant::now() + DRAG_SCROLL_INTERVAL,
                                })
                        });
                        self.set_pointer_cursor(leyline_gfx::PointerCursor::Grabbing)?;
                    }
                    self.tab_drag = Some(drag);
                    self.compose_latest()?;
                    return Ok(());
                }
                leyline_gfx::PointerKind::Release { button: 0x110, .. } => {
                    self.tab_drag_scroll = None;
                    if drag.phase == crate::tab::TabDragPhase::Dragging
                        && let crate::tab::ReorderOutcome::Changed { from, to } = self
                            .current_tabs_mut()
                            .reorder(drag.session, drag.proposed_index)?
                    {
                        tracing::info!(
                            category = "tab_reorder_committed",
                            session_id = drag.session.get(),
                            from,
                            to,
                            "tab reordered by pointer"
                        );
                        self.update_tab_bar();
                    }
                    self.set_pointer_cursor(leyline_gfx::PointerCursor::Text)?;
                    self.compose_latest()?;
                    return Ok(());
                }
                leyline_gfx::PointerKind::Leave { .. } => {
                    self.tab_drag_scroll = None;
                    self.set_pointer_cursor(leyline_gfx::PointerCursor::Text)?;
                    return Ok(());
                }
                _ => self.tab_drag = Some(drag),
            }
        }
        if self.tab_bar.bar.is_some_and(|bar| bar.contains(pixel)) {
            match event.kind {
                leyline_gfx::PointerKind::Press {
                    button: 0x110,
                    serial,
                    ..
                } => {
                    if let Some((id, close)) = self.tab_bar.hit(pixel) {
                        self.switch_to(id)?;
                        if close {
                            self.close_active_tab(ShutdownReason::UserRequested)?;
                        } else {
                            let proposed_index = self
                                .current_tabs_mut()
                                .tabs()
                                .iter()
                                .position(|tab| tab.id == id)
                                .unwrap_or(0);
                            self.tab_drag = Some(crate::tab::TabDrag {
                                session: id,
                                press_serial: serial,
                                origin: [f64::from(pixel[0]), f64::from(pixel[1])],
                                current: [f64::from(pixel[0]), f64::from(pixel[1])],
                                proposed_index,
                                phase: crate::tab::TabDragPhase::Armed,
                            });
                            self.tab_drag_scroll = None;
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
        let (point, owner_point, selection_endpoint) =
            self.published_visual_map.as_ref().map_or_else(
                || (None, None, None),
                |map| match crate::unicode_layout::hit_test(map, &self.layout, pixel) {
                    VisualHit::Cell {
                        physical_point,
                        owner_point,
                        caret,
                    } => {
                        let columns = map.grid.columns.get();
                        let endpoint = if caret.logical_boundary >= columns {
                            (
                                crate::terminal::SelectionPoint {
                                    column: columns - 1,
                                    line: physical_point.line,
                                },
                                crate::terminal::SelectionSide::Right,
                            )
                        } else {
                            (
                                crate::terminal::SelectionPoint {
                                    column: caret.logical_boundary,
                                    line: physical_point.line,
                                },
                                match caret.affinity {
                                    CaretAffinity::Before => crate::terminal::SelectionSide::Left,
                                    CaretAffinity::After => crate::terminal::SelectionSide::Right,
                                },
                            )
                        };
                        (Some(physical_point), Some(owner_point), Some(endpoint))
                    }
                    VisualHit::Outside => (None, None, None),
                },
            );
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
                    let link_point = owner_point.unwrap_or(point);
                    self.link_candidate = self.active_session().hyperlink_at(link_point).map(
                        |(snapshot_generation, hyperlink, _)| LinkCandidate {
                            snapshot_generation,
                            hyperlink,
                            point: link_point,
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
                    let selection_point = selection_endpoint.map_or(point, |endpoint| endpoint.0);
                    let kind = self.click_tracker.register(0x110, selection_point, time_ms);
                    if let Some((point, side)) = selection_endpoint {
                        self.active_session_mut()
                            .start_selection_kind_with_side(kind, point, side)?;
                    } else {
                        self.active_session_mut()
                            .start_selection_kind(kind, selection_point)?;
                    }
                    self.selecting = true;
                    self.selection_point = Some(selection_point);
                    self.selection_kind = Some(kind);
                    self.selection_dragged = false;
                }
            }
            leyline_gfx::PointerKind::Release { button: 0x110, .. } => {
                if let Some(candidate) = self.link_candidate.take() {
                    if let Some(point) = point
                        && let Some((generation, hyperlink, uri)) = self
                            .active_session()
                            .hyperlink_at(owner_point.unwrap_or(point))
                        && candidate.matches(
                            generation,
                            hyperlink,
                            owner_point.unwrap_or(point),
                            self.modifiers,
                        )
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
                    if let Some((selection_point, side)) = selection_endpoint {
                        self.active_session_mut()
                            .update_selection_with_side(selection_point, side)?;
                    } else {
                        self.active_session_mut().update_selection(point)?;
                    }
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
                    let endpoint = selection_endpoint.map_or(point, |endpoint| endpoint.0);
                    self.selection_dragged |= self.selection_point != Some(endpoint);
                    if let Some((endpoint, side)) = selection_endpoint {
                        self.active_session_mut()
                            .update_selection_with_side(endpoint, side)?;
                    } else {
                        self.active_session_mut().update_selection(endpoint)?;
                    }
                    self.selection_point = Some(endpoint);
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

    fn process_tab_drag_scroll(&mut self) -> Result<(), UiRuntimeError> {
        let Some(mut scroll) = self.tab_drag_scroll else {
            return Ok(());
        };
        let now = Instant::now();
        if now < scroll.deadline {
            return Ok(());
        }
        let step = 16_u32.saturating_mul(self.gfx.scale().0).div_ceil(120);
        let previous = self.tab_bar.offset;
        self.tab_bar.offset = if scroll.direction < 0 {
            previous.saturating_sub(step)
        } else {
            previous.saturating_add(step).min(self.tab_bar.max_offset)
        };
        scroll.deadline = now + DRAG_SCROLL_INTERVAL;
        self.tab_drag_scroll = Some(scroll);
        if self.tab_bar.offset == previous {
            return Ok(());
        }
        self.update_tab_bar();
        if let Some(mut drag) = self.tab_drag.take() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let x = drag.current[0].max(0.0).floor() as u32;
            if let Some(index) = self.tab_bar.proposed_index(self.current_tabs(), x) {
                drag.proposed_index = index;
            }
            self.tab_drag = Some(drag);
        }
        self.compose_latest()
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
        self.tab_drag = None;
        self.tab_drag_scroll = None;
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
            .current_tabs_mut()
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
                        .current_tabs_mut()
                        .active_id()
                        .expect("runtime always has an active tab");
                    let search_target = self.pending_search_paste.take();
                    match self.selection.transfer_completed(
                        crate::selection::RequestToken::from_raw(request),
                        target,
                        result,
                        search_target.is_none()
                            && self.app.config().behavior.confirm_multiline_paste,
                        active,
                    ) {
                        crate::selection::PasteTransition::Paste { owner, text } => {
                            if let Some((search_owner, revision)) = search_target {
                                if owner == search_owner
                                    && self.current_tabs().active_id() == Some(owner)
                                    && self.search_focused
                                    && self.active_session().search().is_open()
                                    && self.active_session().search().revision() == revision
                                {
                                    self.active_session_mut().edit_search(
                                        crate::search::SearchEdit::Insert(&text),
                                        Instant::now(),
                                    );
                                    self.compose_latest()?;
                                }
                            } else {
                                self.paste_for_owner(owner, &text)?;
                            }
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
        if !self.activate_session_owner(owner) || self.current_tabs().active_id() != Some(owner) {
            tracing::debug!(
                session_id = owner.get(),
                "stale paste owner is no longer active"
            );
            return Ok(());
        }
        if let Some(tab) = self.current_tabs_mut().get_mut(owner) {
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

fn window_tab_limit(config: &crate::config::EffectiveConfig) -> NonZeroU8 {
    NonZeroU8::new(config.tabs.max_count).expect("validated tab limit is non-zero")
}

fn ignores_key_repeat(action: crate::config::Action) -> bool {
    matches!(
        action,
        crate::config::Action::NewTab
            | crate::config::Action::CloseTab
            | crate::config::Action::CopyClipboard
            | crate::config::Action::PasteClipboard
            | crate::config::Action::PastePrimary
            | crate::config::Action::Search
            | crate::config::Action::CancelSearch
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

fn should_cancel_search(search_open: bool, key: leyline_gfx::LogicalKey) -> bool {
    search_open && key == leyline_gfx::LogicalKey::Escape
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

fn starts_terminal_control_gesture(
    key: leyline_gfx::LogicalKey,
    modifiers: crate::terminal::Modifiers,
    kind: crate::terminal::KeyboardEventKind,
) -> bool {
    modifiers.control
        && kind != crate::terminal::KeyboardEventKind::Release
        && !matches!(key, leyline_gfx::LogicalKey::Modifier { .. })
}

fn terminal_key_owner_changed(expected: Option<u32>, current: Option<u32>) -> bool {
    matches!((expected, current), (Some(expected), Some(current)) if expected != current)
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

fn xkb_text_allowed(ime: &crate::interaction::ImeState) -> bool {
    !ime.has_preedit()
}

fn visible_search_query(
    query: &str,
    cursor_byte: usize,
    max_chars: usize,
    show_cursor: bool,
) -> String {
    if !query.is_char_boundary(cursor_byte) {
        return if show_cursor { "|" } else { "" }.to_owned();
    }
    let query_capacity = max_chars.saturating_sub(usize::from(show_cursor));
    let total_chars = query.chars().count();
    let cursor_char = query[..cursor_byte].chars().count();
    let start_char = cursor_char
        .saturating_sub(query_capacity / 2)
        .min(total_chars.saturating_sub(query_capacity));
    let end_char = start_char.saturating_add(query_capacity).min(total_chars);
    let byte_at = |index: usize| {
        query
            .char_indices()
            .nth(index)
            .map_or(query.len(), |(byte, _)| byte)
    };
    let start_byte = byte_at(start_char);
    let end_byte = byte_at(end_char);
    let mut visible = String::with_capacity(end_byte.saturating_sub(start_byte) + 1);
    visible.push_str(&query[start_byte..cursor_byte]);
    if show_cursor {
        visible.push('|');
    }
    visible.push_str(&query[cursor_byte..end_byte]);
    visible
}

fn search_query_capacity(input_width: u32, cell_width: u16) -> usize {
    usize::try_from(input_width.saturating_sub(20)).unwrap_or(usize::MAX)
        / usize::from(cell_width.max(1))
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

fn resolved_session_title(title: crate::session::SessionTitleDelta, fallback: &str) -> String {
    match title {
        crate::session::SessionTitleDelta::Set(title) if !title.is_empty() => title.to_string(),
        crate::session::SessionTitleDelta::Set(_) | crate::session::SessionTitleDelta::Reset => {
            fallback.to_owned()
        }
    }
}

fn update_session_title(
    current: &mut String,
    title: crate::session::SessionTitleDelta,
    fallback: &str,
) -> bool {
    let resolved = resolved_session_title(title, fallback);
    if *current == resolved {
        return false;
    }
    *current = resolved;
    true
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
        keep_selection_after_release, key_text, renderer_fault_is_process_fatal,
        resolved_session_title, rotate_window_service_order, search_query_capacity,
        select_new_tab_cwd, should_cancel_search, starts_terminal_control_gesture,
        tab_drag_edge_direction, take_matching_pending, take_priority_close_event,
        terminal_key_owner_changed, terminal_modifiers, update_session_title, visible_search_query,
        visual_mapping_changed, xkb_text_allowed,
    };

    #[test]
    fn empty_session_title_uses_launch_fallback() {
        assert_eq!(
            resolved_session_title(crate::session::SessionTitleDelta::Set("".into()), "Shell"),
            "Shell"
        );
        assert_eq!(
            resolved_session_title(crate::session::SessionTitleDelta::Reset, "Shell"),
            "Shell"
        );
        assert_eq!(
            resolved_session_title(
                crate::session::SessionTitleDelta::Set("Editor".into()),
                "Shell"
            ),
            "Editor"
        );
    }

    #[test]
    fn session_title_update_reports_tab_presentation_changes() {
        let mut current = "Shell".to_owned();
        assert!(update_session_title(
            &mut current,
            crate::session::SessionTitleDelta::Set("Codex - ◐".into()),
            "Shell"
        ));
        assert_eq!(current, "Codex - ◐");
        assert!(!update_session_title(
            &mut current,
            crate::session::SessionTitleDelta::Set("Codex - ◐".into()),
            "Shell"
        ));
        assert!(update_session_title(
            &mut current,
            crate::session::SessionTitleDelta::Set("Codex - ◓".into()),
            "Shell"
        ));
        assert_eq!(current, "Codex - ◓");
    }

    fn registry_tab_manager(id: crate::tab::SessionId) -> crate::tab::TabManager {
        let runtime = crate::app::runtime::AppRuntimeBuilder::new(std::sync::Arc::new(
            crate::app::runtime::CountingWake::default(),
        ))
        .build()
        .unwrap();
        let launch = crate::cli::LaunchRequest::Command(crate::cli::CommandSpec {
            program: std::ffi::OsString::from("/bin/true"),
            args: Vec::new(),
        });
        let session = crate::session::TerminalSession::start(
            &launch,
            leyline_pty::SpawnDirectory::open(std::path::Path::new("/tmp")).unwrap(),
            &crate::config::EffectiveConfig::default(),
            crate::terminal::GridSize::new(8, 4).unwrap(),
            &runtime,
        )
        .unwrap();
        crate::tab::TabManager::bootstrap_with_id(
            id,
            session,
            runtime,
            std::num::NonZeroU8::new(4).unwrap(),
        )
    }

    #[test]
    fn production_registry_move_commit_has_one_owner() {
        let source = leyline_gfx::WindowId::from_raw(1).unwrap();
        let target = leyline_gfx::WindowId::from_raw(2).unwrap();
        let mut ids = crate::tab::SessionIdAllocator::default();
        let session = ids.allocate().unwrap();
        let mut registry = super::SessionRegistry {
            windows: std::collections::HashMap::from([(source, registry_tab_manager(session))]),
            locations: std::collections::HashMap::from([(
                session,
                crate::window::SessionLocation::Active { window: source },
            )]),
        };
        let (from, source_empty) = registry
            .commit_move_to_new_window(
                source,
                target,
                session,
                std::num::NonZeroU8::new(4).unwrap(),
            )
            .unwrap();
        assert_eq!(from, 0);
        assert!(source_empty);
        assert!(registry.get(source).unwrap().is_empty());
        assert_eq!(registry.get(target).unwrap().active_id(), Some(session));
        assert_eq!(
            registry.locations.get(&session),
            Some(&crate::window::SessionLocation::Active { window: target })
        );
    }

    #[test]
    fn production_registry_rejects_move_before_mutating_source() {
        let source = leyline_gfx::WindowId::from_raw(1).unwrap();
        let target = leyline_gfx::WindowId::from_raw(2).unwrap();
        let mut ids = crate::tab::SessionIdAllocator::default();
        let session = ids.allocate().unwrap();
        let target_session = ids.allocate().unwrap();
        let mut registry = super::SessionRegistry {
            windows: std::collections::HashMap::from([
                (source, registry_tab_manager(session)),
                (target, registry_tab_manager(target_session)),
            ]),
            locations: std::collections::HashMap::from([
                (
                    session,
                    crate::window::SessionLocation::Active { window: source },
                ),
                (
                    target_session,
                    crate::window::SessionLocation::Active { window: target },
                ),
            ]),
        };
        assert!(matches!(
            registry.commit_move_to_new_window(
                source,
                target,
                session,
                std::num::NonZeroU8::new(4).unwrap(),
            ),
            Err(super::UiRuntimeError::Window(
                crate::window::WindowError::DuplicateWindow(id)
            )) if id == target
        ));
        assert_eq!(registry.get(source).unwrap().active_id(), Some(session));
        assert_eq!(
            registry.locations.get(&session),
            Some(&crate::window::SessionLocation::Active { window: source })
        );
    }

    #[test]
    fn production_registry_rolls_back_location_if_extract_rejects() {
        let source = leyline_gfx::WindowId::from_raw(1).unwrap();
        let target = leyline_gfx::WindowId::from_raw(2).unwrap();
        let mut ids = crate::tab::SessionIdAllocator::default();
        let session = ids.allocate().unwrap();
        let actual_session = ids.allocate().unwrap();
        let mut registry = super::SessionRegistry {
            windows: std::collections::HashMap::from([(
                source,
                registry_tab_manager(actual_session),
            )]),
            locations: std::collections::HashMap::from([(
                session,
                crate::window::SessionLocation::Active { window: source },
            )]),
        };
        assert!(matches!(
            registry.commit_move_to_new_window(
                source,
                target,
                session,
                std::num::NonZeroU8::new(4).unwrap(),
            ),
            Err(super::UiRuntimeError::Tab(
                crate::tab::TabError::UnknownSession(id)
            )) if id == session
        ));
        assert_eq!(
            registry.locations.get(&session),
            Some(&crate::window::SessionLocation::Active { window: source })
        );
        assert!(registry.get(target).is_none());
    }

    #[test]
    fn window_service_cursor_rotates_across_process_rounds() {
        let ids = [1, 2, 3, 4].map(|raw| leyline_gfx::WindowId::from_raw(raw).unwrap());
        let mut first = ids;
        rotate_window_service_order(&mut first, None);
        assert_eq!(first, ids);

        let mut after_second = ids;
        rotate_window_service_order(&mut after_second, Some(ids[1]));
        assert_eq!(after_second, [ids[2], ids[3], ids[0], ids[1]]);

        let mut wrapped = ids;
        rotate_window_service_order(&mut wrapped, Some(ids[3]));
        assert_eq!(wrapped, ids);
    }

    #[test]
    fn close_event_bypasses_queued_bulk_platform_work() {
        let window = leyline_gfx::WindowId::from_raw(1).unwrap();
        let surface = leyline_gfx::SurfaceKey {
            window,
            generation: std::num::NonZeroU64::MIN,
        };
        let mut events = std::collections::VecDeque::from([
            leyline_gfx::RoutedPlatformEvent {
                surface,
                event: leyline_gfx::PlatformEvent::FrameReady,
            },
            leyline_gfx::RoutedPlatformEvent {
                surface,
                event: leyline_gfx::PlatformEvent::CloseRequested,
            },
            leyline_gfx::RoutedPlatformEvent {
                surface,
                event: leyline_gfx::PlatformEvent::FrameReady,
            },
        ]);
        assert!(matches!(
            take_priority_close_event(&mut events).map(|event| event.event),
            Some(leyline_gfx::PlatformEvent::CloseRequested)
        ));
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| matches!(event.event, leyline_gfx::PlatformEvent::FrameReady))
        );
    }

    #[test]
    fn tab_drag_edge_scroll_has_bounded_direction_zones() {
        let bar = crate::tab::PixelRect {
            x: 10,
            y: 0,
            width: 200,
            height: 30,
        };
        assert_eq!(tab_drag_edge_direction(bar, 10, 120), Some(-1));
        assert_eq!(tab_drag_edge_direction(bar, 33, 120), Some(-1));
        assert_eq!(tab_drag_edge_direction(bar, 34, 120), None);
        assert_eq!(tab_drag_edge_direction(bar, 185, 120), None);
        assert_eq!(tab_drag_edge_direction(bar, 186, 120), Some(1));
        assert_eq!(tab_drag_edge_direction(bar, 209, 120), Some(1));
    }

    #[test]
    fn only_device_loss_escalates_a_window_renderer_fault() {
        assert!(renderer_fault_is_process_fatal(
            &leyline_gfx::RendererFault::DeviceLost {
                operation: leyline_gfx::RendererOperation::Present,
            }
        ));
        assert!(!renderer_fault_is_process_fatal(
            &leyline_gfx::RendererFault::SurfaceLost {
                operation: leyline_gfx::RendererOperation::Present,
            }
        ));
        assert!(!renderer_fault_is_process_fatal(
            &leyline_gfx::RendererFault::OutOfDeviceMemory {
                operation: leyline_gfx::RendererOperation::Recreate,
            }
        ));
    }

    #[test]
    fn search_query_window_follows_the_utf8_cursor() {
        assert_eq!(search_query_capacity(112, 9), 10);
        assert_eq!(visible_search_query("abc", 3, 10, true), "abc|");
        assert_eq!(visible_search_query("0123456789", 0, 5, true), "|0123");
        assert_eq!(visible_search_query("0123456789", 5, 5, true), "34|56");
        assert_eq!(visible_search_query("0123456789", 10, 5, true), "6789|");
        assert_eq!(visible_search_query("甲乙丙丁戊", 9, 3, true), "丙|丁");
        assert_eq!(visible_search_query("0123456789", 5, 5, false), "34567");
    }

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
            identity: leyline_gfx::key_identity_from_keysym(keysym),
            keymap_generation: 1,
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
    fn text_input_focus_does_not_suppress_unconsumed_keyboard_text() {
        let mut ime = crate::interaction::ImeState::default();
        ime.activate();
        assert!(xkb_text_allowed(&ime));

        ime.preedit_string("compose".into(), None).unwrap();
        assert!(!xkb_text_allowed(&ime));
    }

    #[test]
    fn pressing_control_alone_does_not_disable_application_shortcuts() {
        let modifiers = crate::terminal::Modifiers {
            control: true,
            shift: true,
            ..crate::terminal::Modifiers::default()
        };
        assert!(!starts_terminal_control_gesture(
            leyline_gfx::LogicalKey::Modifier {
                kind: leyline_gfx::ModifierKind::Control,
                side: leyline_gfx::KeySide::Left,
            },
            modifiers,
            crate::terminal::KeyboardEventKind::Press,
        ));
        assert!(starts_terminal_control_gesture(
            leyline_gfx::LogicalKey::Character('c'),
            modifiers,
            crate::terminal::KeyboardEventKind::Press,
        ));
    }

    #[test]
    fn foreground_job_change_discards_late_key_events() {
        assert!(terminal_key_owner_changed(Some(41), Some(42)));
        assert!(!terminal_key_owner_changed(Some(41), Some(41)));
        assert!(!terminal_key_owner_changed(None, Some(42)));
        assert!(!terminal_key_owner_changed(Some(41), None));
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
    fn escape_closes_an_open_search_regardless_of_editor_focus() {
        assert!(should_cancel_search(true, leyline_gfx::LogicalKey::Escape));
        assert!(!should_cancel_search(
            false,
            leyline_gfx::LogicalKey::Escape
        ));
        assert!(!should_cancel_search(true, leyline_gfx::LogicalKey::Enter));
    }

    #[test]
    fn visual_map_publication_requires_the_exact_committed_frame_key() {
        let key = leyline_gfx::FrameKey {
            snapshot_generation: 4,
            layout_generation: 5,
            font_generation: 6,
            unicode_policy_generation: 7,
        };
        let mut pending = Some((key, "map"));
        let stale = leyline_gfx::CommittedFrameKey {
            frame: leyline_gfx::FrameKey {
                snapshot_generation: 3,
                ..key
            },
            atlas_epoch: 9,
        };
        assert_eq!(take_matching_pending(&mut pending, stale), None);
        assert!(pending.is_none());

        pending = Some((key, "map"));
        let matching = leyline_gfx::CommittedFrameKey {
            frame: key,
            atlas_epoch: 10,
        };
        assert_eq!(take_matching_pending(&mut pending, matching), Some("map"));
        assert!(pending.is_none());
    }

    #[test]
    fn selection_repaint_does_not_invalidate_the_pointer_mapping() {
        let current = crate::unicode_layout::VisualGridMap {
            snapshot_generation: 4,
            policy_generation: 5,
            grid: crate::terminal::GridSize::new(80, 24).unwrap(),
            bidi_enabled: true,
            lines: std::sync::Arc::from([]),
        };
        let repaint = current.clone();
        assert!(!visual_mapping_changed(Some(&current), &repaint));

        let next_snapshot = crate::unicode_layout::VisualGridMap {
            snapshot_generation: 6,
            ..current.clone()
        };
        assert!(visual_mapping_changed(Some(&current), &next_snapshot));
        assert!(visual_mapping_changed(None, &current));
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

fn content_insets(config: &crate::config::EffectiveConfig, tab_count: usize) -> ContentInsets {
    let right = if config.scrollbar.mode == crate::config::ScrollbarMode::Hidden {
        config.window.padding_x
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let gutter = config.scrollbar.hit_width.ceil() as u16;
        config.window.padding_x.max(gutter.saturating_add(2))
    };
    let show_tabs = tab_bar_visible(config, tab_count);
    ContentInsets {
        left: config.window.padding_x,
        right,
        top: config.window.padding_y.saturating_add(if show_tabs {
            config.tabs.bar_height
        } else {
            0
        }),
        bottom: config.window.padding_y,
    }
}

fn tab_bar_visible(config: &crate::config::EffectiveConfig, tab_count: usize) -> bool {
    match config.tabs.visibility {
        crate::config::TabBarVisibility::Always => tab_count > 0,
        crate::config::TabBarVisibility::Multiple => tab_count >= 2,
        crate::config::TabBarVisibility::Never => false,
    }
}

fn rotate_window_service_order(
    ids: &mut [leyline_gfx::WindowId],
    cursor: Option<leyline_gfx::WindowId>,
) {
    ids.sort_unstable();
    let Some(cursor) = cursor else {
        return;
    };
    let start = ids.iter().position(|id| *id > cursor).unwrap_or(0);
    ids.rotate_left(start);
}

fn tab_drag_edge_direction(bar: crate::tab::PixelRect, x: u32, scale_120: u32) -> Option<i8> {
    let edge = 24_u32.saturating_mul(scale_120).div_ceil(120).max(1);
    if x < bar.x.saturating_add(edge) {
        Some(-1)
    } else if x >= bar.x.saturating_add(bar.width).saturating_sub(edge) {
        Some(1)
    } else {
        None
    }
}

fn renderer_fault_is_process_fatal(fault: &leyline_gfx::RendererFault) -> bool {
    matches!(fault, leyline_gfx::RendererFault::DeviceLost { .. })
}

fn take_priority_close_event(
    events: &mut VecDeque<leyline_gfx::RoutedPlatformEvent>,
) -> Option<leyline_gfx::RoutedPlatformEvent> {
    let index = events
        .iter()
        .position(|routed| matches!(&routed.event, PlatformEvent::CloseRequested))?;
    events.remove(index)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn requested_normal_size(
    config: &crate::config::EffectiveConfig,
    metrics: leyline_text::CellMetrics,
) -> Result<leyline_gfx::LogicalSize, UiRuntimeError> {
    let width = u32::from(config.window.columns)
        .checked_mul(u32::from(metrics.width_px.get()))
        .and_then(|value| value.checked_add(u32::from(config.window.padding_x) * 2))
        .and_then(|value| {
            (config.scrollbar.mode != crate::config::ScrollbarMode::Hidden)
                .then(|| config.scrollbar.hit_width.ceil() as u32 + 2)
                .map_or(Some(value), |gutter| value.checked_add(gutter))
        })
        .ok_or_else(|| UiRuntimeError::Grid("requested window width overflow".into()))?;
    let rows = u32::from(config.window.lines)
        .checked_mul(u32::from(metrics.height_px.get()))
        .ok_or_else(|| UiRuntimeError::Grid("requested window height overflow".into()))?;
    let chrome = u32::from(config.window.padding_y)
        .checked_mul(2)
        .and_then(|value| {
            value.checked_add(if tab_bar_visible(config, 1) {
                u32::from(config.tabs.bar_height)
            } else {
                0
            })
        })
        .ok_or_else(|| UiRuntimeError::Grid("requested window height overflow".into()))?;
    let height = rows
        .checked_add(chrome)
        .ok_or_else(|| UiRuntimeError::Grid("requested window height overflow".into()))?;
    Ok(leyline_gfx::LogicalSize { width, height })
}

fn take_matching_pending<T>(
    pending: &mut Option<(leyline_gfx::FrameKey, T)>,
    committed: leyline_gfx::CommittedFrameKey,
) -> Option<T> {
    pending
        .take()
        .and_then(|(key, value)| (key == committed.frame).then_some(value))
}

fn visual_mapping_changed(current: Option<&VisualGridMap>, next: &VisualGridMap) -> bool {
    current.is_none_or(|current| {
        current.snapshot_generation != next.snapshot_generation
            || current.policy_generation != next.policy_generation
            || current.grid != next.grid
            || current.bidi_enabled != next.bidi_enabled
    })
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
    Tab(#[from] crate::tab::TabError),
    #[error(transparent)]
    Window(#[from] crate::window::WindowError),
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
    UnicodeLayout(#[from] crate::unicode_layout::UnicodeLayoutError),
    #[error(transparent)]
    Ime(#[from] crate::interaction::ImeError),
}

impl ClassifiedError for UiRuntimeError {
    #[allow(clippy::unnested_or_patterns)]
    fn category(&self) -> ErrorCategory {
        match self {
            Self::Init(GfxInitError::Environment(_))
            | Self::Graphics(GfxError::Initialization(GfxInitError::Environment(_)))
            | Self::SessionStart(_)
            | Self::SpawnDirectory(_)
            | Self::Graphics(GfxError::Platform(_)) => ErrorCategory::Environment,
            Self::Init(GfxInitError::Platform(_))
            | Self::Graphics(GfxError::Initialization(GfxInitError::Platform(_)))
            | Self::Ime(_) => ErrorCategory::Platform,
            Self::Init(GfxInitError::Device(_))
            | Self::Graphics(GfxError::Initialization(GfxInitError::Device(_)))
            | Self::Graphics(GfxError::Renderer(_)) => ErrorCategory::Renderer,
            Self::Graphics(GfxError::Internal(_) | GfxError::Capacity(_))
            | Self::App(_)
            | Self::Wake(_)
            | Self::Session(_)
            | Self::Tab(_)
            | Self::Window(_)
            | Self::Runtime(_)
            | Self::Grid(_)
            | Self::Text(_)
            | Self::Layout(_)
            | Self::Compose(_)
            | Self::UnicodeLayout(_) => ErrorCategory::Internal,
        }
    }
}

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    env,
    ffi::c_void,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    rc::Rc,
    time::{Duration, Instant},
};

const RETAINED_INPUT_CAPACITY: usize = 256;
const INPUT_DRAIN_BUDGET: usize = 64;

use rustix::{
    event::{PollFd, PollFlags, poll},
    time::Timespec,
};

use smithay_client_toolkit::reexports::csd_frame::WindowState as XdgWindowState;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    data_device_manager::{
        DataDeviceManagerState,
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer, SelectionOffer},
        data_source::{CopyPasteSource, DataSourceHandler},
    },
    delegate_compositor, delegate_data_device, delegate_keyboard, delegate_output,
    delegate_pointer, delegate_primary_selection, delegate_seat, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    primary_selection::{
        PrimarySelectionManagerState,
        device::{PrimarySelectionDevice, PrimarySelectionDeviceHandler},
        offer::PrimarySelectionOffer,
        selection::{PrimarySelectionSource, PrimarySelectionSourceHandler},
    },
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{
            KeyEvent, KeyboardHandler, Keymap, Keysym, Modifiers, RawModifiers, RepeatInfo,
        },
        pointer::{
            CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
        },
    },
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{Shm, ShmHandler},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::ObjectId,
    backend::WaylandError,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_data_device, wl_data_device_manager::DndAction, wl_data_source, wl_keyboard, wl_output,
        wl_pointer, wl_registry, wl_seat, wl_surface,
    },
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    text_input::zv3::client::{
        zwp_text_input_manager_v3::ZwpTextInputManagerV3,
        zwp_text_input_v3::{self, ContentHint, ContentPurpose, ZwpTextInputV3},
    },
    viewporter::client::{
        wp_viewport::{self, WpViewport},
        wp_viewporter::WpViewporter,
    },
};
use xkbcommon::xkb;

use crate::decor::{Libdecor, LibdecorContext, ResizeEdge};
use crate::{
    ClipboardEvent, GfxInitError, InputSerial, KeyInput, KeyState, LogicalSize, ModifierMask,
    ModifiersState, PlatformEvent, PointerCursor, PointerInput, PointerKind, Scale120, SeatToken,
    SelectionTarget, SerialKind, TextInputContext, TextInputEvent, TextInputPurpose, WindowState,
    key_identity_from_keysym, logical_key_from_keysym,
};

const CONTENT_RESIZE_MARGIN: f64 = 6.0;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PressedModifiers(u16);

impl PressedModifiers {
    const SHIFT_LEFT: u16 = 1 << 0;
    const SHIFT_RIGHT: u16 = 1 << 1;
    const CONTROL_LEFT: u16 = 1 << 2;
    const CONTROL_RIGHT: u16 = 1 << 3;
    const ALT_LEFT: u16 = 1 << 4;
    const ALT_RIGHT: u16 = 1 << 5;
    const SUPER_LEFT: u16 = 1 << 6;
    const SUPER_RIGHT: u16 = 1 << 7;
    const ALT_GRAPH: u16 = 1 << 8;

    fn update(&mut self, keysym: u32, state: KeyState) {
        let bit = match keysym {
            0xffe1 => Self::SHIFT_LEFT,
            0xffe2 => Self::SHIFT_RIGHT,
            0xffe3 => Self::CONTROL_LEFT,
            0xffe4 => Self::CONTROL_RIGHT,
            0xffe9 => Self::ALT_LEFT,
            0xffea => Self::ALT_RIGHT,
            0xffeb => Self::SUPER_LEFT,
            0xffec => Self::SUPER_RIGHT,
            0xfe03 => Self::ALT_GRAPH,
            _ => return,
        };
        match state {
            KeyState::Pressed => self.0 |= bit,
            KeyState::Released => self.0 &= !bit,
        }
    }

    const fn merge(self, reported: ModifiersState) -> ModifiersState {
        ModifiersState {
            shift: reported.shift || self.0 & (Self::SHIFT_LEFT | Self::SHIFT_RIGHT) != 0,
            control: reported.control || self.0 & (Self::CONTROL_LEFT | Self::CONTROL_RIGHT) != 0,
            alt: reported.alt || self.0 & (Self::ALT_LEFT | Self::ALT_RIGHT) != 0,
            super_key: reported.super_key || self.0 & (Self::SUPER_LEFT | Self::SUPER_RIGHT) != 0,
            alt_graph: reported.alt_graph || self.0 & Self::ALT_GRAPH != 0,
            caps_lock: reported.caps_lock,
            num_lock: reported.num_lock,
        }
    }

    const fn physical_control(self) -> bool {
        self.0 & (Self::CONTROL_LEFT | Self::CONTROL_RIGHT) != 0
    }

    const fn physical_alt(self) -> bool {
        self.0 & (Self::ALT_LEFT | Self::ALT_RIGHT) != 0
    }
}

fn shortcut_modifiers(effective: ModifiersState, pressed: PressedModifiers) -> ModifierMask {
    let mut mask = ModifierMask::empty();
    if effective.shift {
        mask.insert(ModifierMask::SHIFT);
    }
    if effective.control && (!effective.alt_graph || pressed.physical_control()) {
        mask.insert(ModifierMask::CONTROL);
    }
    if effective.alt && (!effective.alt_graph || pressed.physical_alt()) {
        mask.insert(ModifierMask::ALT);
    }
    if effective.super_key {
        mask.insert(ModifierMask::SUPER);
    }
    mask
}

#[derive(Default)]
struct PendingEvents {
    close: bool,
    configured: Option<(LogicalSize, Scale120, WindowState)>,
    scale: Option<Scale120>,
    frame_ready: bool,
    suspended: Option<bool>,
    input: VecDeque<PlatformEvent>,
    input_overflowed: bool,
    resize: Option<(wl_seat::WlSeat, u32, ResizeEdge)>,
}

impl PendingEvents {
    fn can_dispatch_callbacks(&self) -> bool {
        self.input.is_empty()
    }

    fn push_input(&mut self, event: PlatformEvent) {
        if matches!(
            event,
            PlatformEvent::Pointer(PointerInput {
                kind: PointerKind::Motion { .. },
                ..
            })
        ) && self.input.back().is_some_and(|pending| {
            matches!(
                pending,
                PlatformEvent::Pointer(PointerInput {
                    kind: PointerKind::Motion { .. },
                    ..
                })
            )
        }) {
            // Only the newest position matters until an ordered button/axis event intervenes.
            if let Some(pending) = self.input.back_mut() {
                *pending = event;
            }
            return;
        }
        if self.input.len() == RETAINED_INPUT_CAPACITY {
            self.input_overflowed = true;
        } else {
            self.input.push_back(event);
        }
    }

    fn drain_input(&mut self, output: &mut Vec<PlatformEvent>) {
        for _ in 0..INPUT_DRAIN_BUDGET {
            let Some(event) = self.input.pop_front() else {
                break;
            };
            output.push(event);
        }
    }
}

#[derive(Default)]
struct KeyRepeatState {
    settings: Option<(Duration, Duration)>,
    active: Option<ActiveKeyRepeat>,
}

struct ActiveKeyRepeat {
    serial: u32,
    event: KeyEvent,
    next: Instant,
    interval: Duration,
}

impl KeyRepeatState {
    fn update_info(&mut self, info: RepeatInfo) {
        self.settings = match info {
            RepeatInfo::Repeat { rate, delay } => Some((
                Duration::from_millis(u64::from(delay)),
                Duration::from_micros((1_000_000 / u64::from(rate.get())).max(1)),
            )),
            RepeatInfo::Disable => None,
        };
        if self.settings.is_none() {
            self.active = None;
        }
    }

    fn press(&mut self, serial: u32, event: KeyEvent, now: Instant) {
        self.active = None;
        let Some((delay, interval)) = self.settings else {
            return;
        };
        if matches!(
            logical_key_from_keysym(event.keysym.raw()),
            crate::LogicalKey::Modifier { .. }
                | crate::LogicalKey::CapsLock
                | crate::LogicalKey::NumLock
                | crate::LogicalKey::Menu
                | crate::LogicalKey::Unidentified(_)
        ) {
            return;
        }
        self.active = Some(ActiveKeyRepeat {
            serial,
            event,
            next: now + delay,
            interval,
        });
    }

    fn release(&mut self, raw_code: u32) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.event.raw_code == raw_code)
        {
            self.active = None;
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.active.as_ref().map(|active| active.next)
    }

    fn take_due(&mut self, now: Instant) -> Option<(u32, KeyEvent)> {
        let active = self.active.as_mut().filter(|active| active.next <= now)?;
        active.next = now + active.interval;
        Some((active.serial, active.event.clone()))
    }

    fn cancel(&mut self) {
        self.active = None;
    }
}

fn min_timeout(timeout: Option<Duration>, deadline: Option<Instant>) -> Option<Duration> {
    let repeat = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
    match (timeout, repeat) {
        (Some(timeout), Some(repeat)) => Some(timeout.min(repeat)),
        (Some(timeout), None) => Some(timeout),
        (None, Some(repeat)) => Some(repeat),
        (None, None) => None,
    }
}

pub(crate) struct WaylandConnectionHost {
    connection: Connection,
    globals: Rc<GlobalList>,
    event_queue: Option<EventQueue<WaylandState>>,
    state: WaylandState,
    flush_blocked: bool,
}

pub(crate) struct WaylandWindow {
    host: Rc<RefCell<WaylandConnectionHost>>,
    state: Rc<RefCell<WindowEventState>>,
    window: Option<Window>,
    surface: wl_surface::WlSurface,
    libdecor: Option<Libdecor>,
    _fractional_scale: Option<WpFractionalScaleV1>,
    viewport: Option<WpViewport>,
}

impl Drop for WaylandWindow {
    fn drop(&mut self) {
        // libdecor keeps raw references to the display and surface, so release it before Rust
        // drops the Wayland objects declared earlier in this struct.
        drop(self.libdecor.take());
        let mut host = self.host.borrow_mut();
        let surface = self.surface.id();
        host.state.windows.remove(&surface);
        host.state
            .fractional_windows
            .retain(|_, target| *target != surface);
        if host.state.keyboard_focus.as_ref() == Some(&surface) {
            host.state.keyboard_focus = None;
            host.state.key_repeat.cancel();
        }
        if host.state.pointer_focus.as_ref() == Some(&surface) {
            host.state.pointer_focus = None;
        }
        leave_text_input_focus(&mut host.state.text_input_focus, &surface);
        host.trace_snapshot();
    }
}

impl WaylandConnectionHost {
    #[allow(clippy::too_many_lines)]
    fn connect() -> Result<Rc<RefCell<Self>>, GfxInitError> {
        let connection = connect_to_env()?;
        let (globals, event_queue) = registry_queue_init(&connection).map_err(|error| {
            GfxInitError::Platform(format!("cannot read Wayland globals: {error}"))
        })?;
        let globals = Rc::new(globals);
        let advertised = globals.contents().clone_list();
        for required in ["wl_compositor", "xdg_wm_base"] {
            if !advertised.iter().any(|global| global.interface == required) {
                return Err(GfxInitError::Platform(format!(
                    "Wayland compositor does not advertise required {required}"
                )));
            }
        }
        let qh = event_queue.handle();
        let compositor = CompositorState::bind(&globals, &qh).map_err(|error| {
            GfxInitError::Platform(format!("cannot bind wl_compositor: {error}"))
        })?;
        let shm = Shm::bind(&globals, &qh)
            .map_err(|error| GfxInitError::Platform(format!("cannot bind wl_shm: {error}")))?;
        let shell = XdgShell::bind(&globals, &qh)
            .map_err(|error| GfxInitError::Platform(format!("cannot bind xdg-shell: {error}")))?;
        let cursor_surface = compositor.create_surface(&qh);
        let data_device_manager = DataDeviceManagerState::bind(&globals, &qh).map_err(|error| {
            GfxInitError::Platform(format!(
                "cannot bind required wl_data_device_manager: {error}"
            ))
        })?;
        let state = WaylandState {
            compositor,
            shell,
            outputs: OutputState::new(&globals, &qh),
            seats: SeatState::new(&globals, &qh),
            shm,
            keyboard: None,
            keyboard_focus: None,
            pointer: None,
            pointer_seat: None,
            cursor_surface,
            pointer_icon: None,
            pointer_focus: None,
            data_device_manager: Some(data_device_manager),
            data_device: None,
            clipboard_offer: None,
            clipboard_source: None,
            primary_selection_manager: PrimarySelectionManagerState::bind(&globals, &qh).ok(),
            primary_device: None,
            primary_offer: None,
            primary_source: None,
            text_input_manager: globals
                .bind::<ZwpTextInputManagerV3, _, _>(&qh, 1..=2, ())
                .ok(),
            text_input: None,
            text_input_focus: None,
            text_input_commits: 0,
            modifiers: ModifiersState::default(),
            pressed_modifiers: PressedModifiers::default(),
            key_repeat: KeyRepeatState::default(),
            shortcut_digit_rows: HashMap::new(),
            keymap_generation: 0,
            xkb_keymap: None,
            xkb_state: None,
            windows: HashMap::new(),
            fractional_windows: HashMap::new(),
            seat_token: SeatToken::new(0, 1),
        };
        Ok(Rc::new(RefCell::new(Self {
            connection,
            globals,
            event_queue: Some(event_queue),
            state,
            flush_blocked: false,
        })))
    }

    fn dispatch_pending(&mut self) -> Result<(), String> {
        if self
            .state
            .windows
            .values()
            .any(|window| !window.borrow().pending.can_dispatch_callbacks())
        {
            return Ok(());
        }
        let mut queue = self
            .event_queue
            .take()
            .expect("Wayland event queue installed");
        let result = queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| format!("Wayland dispatch failed: {error}"));
        self.event_queue = Some(queue);
        self.state.emit_due_key_repeat(Instant::now());
        result.map(|_| ())
    }

    fn queue_handle(&self) -> QueueHandle<WaylandState> {
        self.event_queue
            .as_ref()
            .expect("Wayland event queue installed")
            .handle()
    }

    fn trace_snapshot(&self) {
        tracing::debug!(
            category = "wayland_host_snapshot",
            registries = 1,
            event_queues = 1,
            seat_states = 1,
            surfaces = self.state.windows.len(),
            clipboard_device = self.state.data_device.is_some(),
            primary_device = self.state.primary_device.is_some(),
            "Wayland connection host resource snapshot"
        );
    }

    fn roundtrip(&mut self) -> Result<(), String> {
        let mut queue = self
            .event_queue
            .take()
            .expect("Wayland event queue installed");
        let result = queue
            .roundtrip(&mut self.state)
            .map_err(|error| format!("Wayland roundtrip failed: {error}"));
        self.event_queue = Some(queue);
        result.map(|_| ())
    }

    fn flush(&mut self) -> Result<(), String> {
        match self.connection.flush() {
            Ok(()) => self.flush_blocked = false,
            Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                self.flush_blocked = true;
            }
            Err(error) => return Err(format!("Wayland flush failed: {error}")),
        }
        Ok(())
    }

    fn poll_read(
        &mut self,
        wake: Option<BorrowedFd<'_>>,
        timeout: Option<Duration>,
    ) -> Result<(), String> {
        if self
            .state
            .windows
            .values()
            .any(|window| !window.borrow().pending.input.is_empty())
        {
            return Ok(());
        }
        self.flush()?;
        let timeout = min_timeout(timeout, self.state.key_repeat.deadline());
        let queue = self
            .event_queue
            .as_mut()
            .expect("Wayland event queue installed");
        let Some(read_guard) = queue.prepare_read() else {
            return self.dispatch_pending();
        };
        let mut interest = PollFlags::IN | PollFlags::ERR | PollFlags::HUP;
        if self.flush_blocked {
            interest |= PollFlags::OUT;
        }
        let mut descriptors = vec![PollFd::from_borrowed_fd(self.connection.as_fd(), interest)];
        if let Some(wake) = wake {
            descriptors.push(PollFd::from_borrowed_fd(
                wake,
                PollFlags::IN | PollFlags::ERR,
            ));
        }
        let timeout = timeout.map(|value| Timespec {
            tv_sec: i64::try_from(value.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(value.subsec_nanos()),
        });
        loop {
            match poll(&mut descriptors, timeout.as_ref()) {
                Ok(_) => break,
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(format!("UI poll failed: {error}")),
            }
        }
        let readiness = descriptors[0].revents();
        if readiness.intersects(PollFlags::ERR | PollFlags::HUP) {
            return Err("Wayland compositor disconnected".into());
        }
        if readiness.contains(PollFlags::IN) {
            read_guard
                .read()
                .map_err(|error| format!("Wayland socket read failed: {error}"))?;
            tracing::trace!(
                category = "wayland_socket_read",
                "read Wayland socket events"
            );
        }
        if readiness.contains(PollFlags::OUT) {
            self.flush()?;
        }
        Ok(())
    }
}

impl WaylandWindow {
    #[allow(clippy::too_many_lines)]
    pub(crate) fn connect(title: &str, default_size: LogicalSize) -> Result<Self, GfxInitError> {
        Self::connect_on(WaylandConnectionHost::connect()?, None, title, default_size)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn connect_on(
        host: Rc<RefCell<WaylandConnectionHost>>,
        libdecor_context: Option<std::rc::Rc<LibdecorContext>>,
        title: &str,
        default_size: LogicalSize,
    ) -> Result<Self, GfxInitError> {
        let mut shared = host.borrow_mut();
        let connection = shared.connection.clone();
        let globals = Rc::clone(&shared.globals);
        let advertised = globals.contents().clone_list();
        let qh = shared.queue_handle();
        let compositor = &shared.state.compositor;
        let shell = &shared.state.shell;
        let surface = compositor.create_surface(&qh);
        let local = Rc::new(RefCell::new(WindowEventState::new(default_size)));
        let has_server_decor = advertised
            .iter()
            .any(|global| global.interface == "zxdg_decoration_manager_v1");
        let fractional_pair = globals
            .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
            .ok()
            .zip(globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()).ok());
        let (fractional_scale, viewport) =
            fractional_pair.map_or((None, None), |(manager, viewporter)| {
                (
                    Some(manager.get_fractional_scale(&surface, &qh, ())),
                    Some(viewporter.get_viewport(&surface, &qh, ())),
                )
            });
        let (window, libdecor) = if has_server_decor {
            let window =
                shell.create_window(surface.clone(), WindowDecorations::RequestServer, &qh);
            window.set_title(title);
            window.set_app_id("io.leyline.Leyline");
            window.set_min_size(Some((160, 90)));
            window.commit();
            (Some(window), None)
        } else {
            (
                None,
                Some(
                    libdecor_context
                        .map_or_else(
                            || Libdecor::new(&connection, &surface, title, default_size),
                            |context| Libdecor::new_on(context, &surface, title, default_size),
                        )
                        .map_err(|error| {
                            GfxInitError::Platform(format!(
                                "cannot create required libdecor window: {error}"
                            ))
                        })?,
                ),
            )
        };
        shared.state.windows.insert(surface.id(), Rc::clone(&local));
        local.borrow_mut().libdecor_resize_fallback = !has_server_decor;
        if let Some(fractional) = fractional_scale.as_ref() {
            shared
                .state
                .fractional_windows
                .insert(fractional.id(), surface.id());
        }
        shared.trace_snapshot();
        drop(shared);
        Ok(Self {
            host,
            state: local,
            window,
            surface,
            libdecor,
            _fractional_scale: fractional_scale,
            viewport,
        })
    }

    pub(crate) fn libdecor_context(&self) -> Option<std::rc::Rc<LibdecorContext>> {
        self.libdecor.as_ref().map(Libdecor::context)
    }

    pub(crate) fn host(&self) -> Rc<RefCell<WaylandConnectionHost>> {
        Rc::clone(&self.host)
    }

    pub(crate) fn dispatch_pending(&mut self) -> Result<(), String> {
        // Drain the retained round before admitting another callback batch. Otherwise a busy
        // input source can refill the bounded queue faster than the UI consumes its old events.
        if !self.state.borrow().pending.can_dispatch_callbacks() {
            return Ok(());
        }
        if let Some(decor) = self.libdecor.as_mut() {
            decor.dispatch(0)?;
            if let Some((size, state)) = decor.take_configured() {
                let mut local = self.state.borrow_mut();
                local.logical_size = size;
                local.configured = true;
                let scale = local.scale;
                local.pending.configured = Some((size, scale, state));
            }
            self.state.borrow_mut().pending.close |= decor.take_close();
            if decor.take_commit_requested() {
                self.surface.commit();
            }
        }
        self.host.borrow_mut().dispatch_pending()?;
        if let Some((seat, serial, edge)) = self.state.borrow_mut().pending.resize.take()
            && let Some(decor) = self.libdecor.as_mut()
        {
            decor.resize(&seat, serial, edge);
        }
        Ok(())
    }

    pub(crate) fn roundtrip(&mut self) -> Result<(), String> {
        if let Some(decor) = self.libdecor.as_mut() {
            decor.dispatch(50)?;
            if let Some((size, state)) = decor.take_configured() {
                let mut local = self.state.borrow_mut();
                local.logical_size = size;
                local.configured = true;
                let scale = local.scale;
                local.pending.configured = Some((size, scale, state));
            }
            if decor.take_commit_requested() {
                self.surface.commit();
            }
            return Ok(());
        }
        self.host.borrow_mut().roundtrip()
    }

    pub(crate) fn flush(&mut self) -> Result<(), String> {
        self.host.borrow_mut().flush()
    }

    pub(crate) fn poll_read(
        &mut self,
        wake: Option<BorrowedFd<'_>>,
        timeout: Option<Duration>,
    ) -> Result<(), String> {
        if !self.state.borrow().pending.input.is_empty() {
            return Ok(());
        }
        let result = self.host.borrow_mut().poll_read(wake, timeout);
        if let Some(decor) = self.libdecor.as_mut() {
            decor.dispatch(0)?;
            if decor.take_commit_requested() {
                self.surface.commit();
            }
        }
        result
    }

    pub(crate) fn take_events(&mut self, output: &mut Vec<PlatformEvent>) -> Result<(), String> {
        let mut local = self.state.borrow_mut();
        if local.pending.input_overflowed {
            return Err(format!(
                "Wayland retained input queue exceeded {RETAINED_INPUT_CAPACITY} events"
            ));
        }
        if local.pending.close {
            local.pending.close = false;
            output.push(PlatformEvent::CloseRequested);
        }
        if let Some((logical_size, scale, state)) = local.pending.configured.take() {
            output.push(PlatformEvent::Configured {
                logical_size,
                scale,
                state,
            });
        }
        if let Some(scale) = local.pending.scale.take() {
            output.push(PlatformEvent::ScaleChanged { scale });
        }
        if local.pending.frame_ready {
            local.pending.frame_ready = false;
            output.push(PlatformEvent::FrameReady);
        }
        if let Some(suspended) = local.pending.suspended.take() {
            output.push(if suspended {
                PlatformEvent::SurfaceSuspended
            } else {
                PlatformEvent::SurfaceResumed
            });
        }
        local.pending.drain_input(output);
        Ok(())
    }

    pub(crate) fn request_frame(&self) {
        self.surface
            .frame(&self.host.borrow().queue_handle(), self.surface.clone());
    }

    pub(crate) fn commit(&self) {
        let local = self.state.borrow();
        if let Some(viewport) = self.viewport.as_ref() {
            viewport.set_destination(
                i32::try_from(local.logical_size.width).unwrap_or(i32::MAX),
                i32::try_from(local.logical_size.height).unwrap_or(i32::MAX),
            );
        } else {
            let integer_scale = (local.scale.0 / 120).max(1);
            self.surface
                .set_buffer_scale(i32::try_from(integer_scale).unwrap_or(i32::MAX));
        }
        self.surface.commit();
    }

    pub(crate) fn text_input_available(&self) -> bool {
        self.host.borrow().state.text_input_manager.is_some()
    }

    pub(crate) fn enable_text_input(
        &mut self,
        context: TextInputContext,
    ) -> Result<Option<u32>, String> {
        if !self.owns_text_input_focus() {
            tracing::debug!(
                category = "text_input_request_rejected",
                operation = "enable",
                local_focus = self.state.borrow().text_input_focused,
                "ignored text-input request from a surface without focus"
            );
            return Ok(None);
        }
        let Some(input) = self.host.borrow().state.text_input.clone() else {
            return Ok(None);
        };
        let purpose = context.purpose;
        let surrounding_bytes = context.surrounding_text.len();
        let rectangle = context.rectangle;
        input.enable();
        input.set_content_type(
            ContentHint::None,
            match context.purpose {
                TextInputPurpose::Terminal => ContentPurpose::Terminal,
                TextInputPurpose::Normal => ContentPurpose::Normal,
            },
        );
        input.set_surrounding_text(
            context.surrounding_text,
            context.cursor_byte,
            context.anchor_byte,
        );
        input.set_cursor_rectangle(
            context.rectangle.x,
            context.rectangle.y,
            context.rectangle.width.max(1),
            context.rectangle.height.max(1),
        );
        input.commit();
        self.surface.commit();
        let serial = self.host.borrow_mut().state.bump_text_input_commit();
        tracing::debug!(
            category = "text_input_request",
            operation = "enable",
            ?serial,
            ?purpose,
            surrounding_bytes,
            ?rectangle,
            "committed text-input state"
        );
        self.flush()?;
        Ok(serial)
    }

    pub(crate) fn update_text_input(
        &mut self,
        context: TextInputContext,
    ) -> Result<Option<u32>, String> {
        if !self.owns_text_input_focus() {
            tracing::debug!(
                category = "text_input_request_rejected",
                operation = "update",
                local_focus = self.state.borrow().text_input_focused,
                "ignored text-input request from a surface without focus"
            );
            return Ok(None);
        }
        let Some(input) = self.host.borrow().state.text_input.clone() else {
            return Ok(None);
        };
        let purpose = context.purpose;
        let surrounding_bytes = context.surrounding_text.len();
        let rectangle = context.rectangle;
        input.set_content_type(
            ContentHint::None,
            match context.purpose {
                TextInputPurpose::Terminal => ContentPurpose::Terminal,
                TextInputPurpose::Normal => ContentPurpose::Normal,
            },
        );
        input.set_surrounding_text(
            context.surrounding_text,
            context.cursor_byte,
            context.anchor_byte,
        );
        input.set_cursor_rectangle(
            context.rectangle.x,
            context.rectangle.y,
            context.rectangle.width.max(1),
            context.rectangle.height.max(1),
        );
        input.commit();
        self.surface.commit();
        let serial = self.host.borrow_mut().state.bump_text_input_commit();
        tracing::trace!(
            category = "text_input_request",
            operation = "update",
            ?serial,
            ?purpose,
            surrounding_bytes,
            ?rectangle,
            "committed text-input state"
        );
        self.flush()?;
        Ok(serial)
    }

    pub(crate) fn disable_text_input(&mut self) -> Result<Option<u32>, String> {
        // The text-input object is shared by every surface on this seat. A delayed keyboard-leave
        // from an old window must not disable the surface which currently owns text-input focus.
        if !self.owns_text_input_focus() {
            tracing::debug!(
                category = "text_input_request_rejected",
                operation = "disable",
                local_focus = self.state.borrow().text_input_focused,
                "ignored text-input request from a surface without focus"
            );
            return Ok(None);
        }
        let Some(input) = self.host.borrow().state.text_input.clone() else {
            return Ok(None);
        };
        input.set_surrounding_text(String::new(), 0, 0);
        input.set_content_type(ContentHint::None, ContentPurpose::Terminal);
        input.disable();
        input.commit();
        self.surface.commit();
        let serial = self.host.borrow_mut().state.bump_text_input_commit();
        tracing::debug!(
            category = "text_input_request",
            operation = "disable",
            ?serial,
            "committed text-input state"
        );
        self.flush()?;
        Ok(serial)
    }

    fn owns_text_input_focus(&self) -> bool {
        let locally_focused = self.state.borrow().text_input_focused;
        text_input_request_owned_by(
            locally_focused,
            self.host.borrow().state.text_input_focus.as_ref(),
            &self.surface.id(),
        )
    }

    pub(crate) fn publish_selection(
        &mut self,
        target: SelectionTarget,
        source_id: u64,
        serial: InputSerial,
    ) -> Result<bool, String> {
        const MIMES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
        let mut host = self.host.borrow_mut();
        if serial.seat != host.state.seat_token {
            return Err("selection publish rejected a stale seat serial".into());
        }
        let qh = host.queue_handle();
        match target {
            SelectionTarget::Clipboard => {
                let Some(device) = host.state.data_device.as_ref() else {
                    return Ok(false);
                };
                let Some(manager) = host.state.data_device_manager.as_ref() else {
                    return Ok(false);
                };
                let source = manager.create_copy_paste_source(&qh, MIMES);
                source.set_selection(device, serial.value);
                host.state.clipboard_source = Some((source_id, source));
            }
            SelectionTarget::Primary => {
                let (Some(manager), Some(device)) = (
                    host.state.primary_selection_manager.as_ref(),
                    host.state.primary_device.as_ref(),
                ) else {
                    return Ok(false);
                };
                let source = manager.create_selection_source(&qh, MIMES);
                source.set_selection(device, serial.value);
                host.state.primary_source = Some((source_id, source));
            }
        }
        host.flush()?;
        Ok(true)
    }

    pub(crate) fn receive_selection(
        &mut self,
        target: SelectionTarget,
    ) -> Result<Option<OwnedFd>, String> {
        let choose_mime = |mimes: &[String]| {
            ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"]
                .iter()
                .find_map(|wanted| mimes.iter().find(|mime| mime.as_str() == *wanted).cloned())
        };
        let host = self.host.borrow();
        let fd = match target {
            SelectionTarget::Clipboard => {
                let Some(offer) = host.state.clipboard_offer.as_ref() else {
                    return Ok(None);
                };
                let Some(mime) = offer.with_mime_types(choose_mime) else {
                    return Ok(None);
                };
                offer.receive(mime).ok().map(OwnedFd::from)
            }
            SelectionTarget::Primary => {
                let Some(offer) = host.state.primary_offer.as_ref() else {
                    return Ok(None);
                };
                let Some(mime) = offer.with_mime_types(choose_mime) else {
                    return Ok(None);
                };
                offer.receive(mime).ok().map(OwnedFd::from)
            }
        };
        drop(host);
        if fd.is_some() {
            self.flush()?;
        }
        Ok(fd)
    }
    pub(crate) fn set_title(&mut self, title: &str) {
        if let Some(window) = self.window.as_ref() {
            window.set_title(title);
        }
        // Validation in the safe facade already excludes NUL.
        if let Some(decor) = self.libdecor.as_mut() {
            let _ = decor.set_title(title);
        }
    }
    pub(crate) fn set_maximized(&mut self, maximized: bool) {
        if let Some(window) = self.window.as_ref() {
            if maximized {
                window.set_maximized();
            } else {
                window.unset_maximized();
            }
        }
        if let Some(decor) = self.libdecor.as_mut() {
            decor.set_maximized(maximized);
        }
    }
    pub(crate) fn set_fullscreen(&mut self, fullscreen: bool) {
        if let Some(window) = self.window.as_ref() {
            if fullscreen {
                window.set_fullscreen(None);
            } else {
                window.unset_fullscreen();
            }
        }
        if let Some(decor) = self.libdecor.as_mut() {
            decor.set_fullscreen(fullscreen);
        }
    }
    pub(crate) fn set_pointer_cursor(&mut self, cursor: PointerCursor) {
        let edge = {
            let mut local = self.state.borrow_mut();
            local.pointer_cursor = cursor;
            local.pointer_resize_edge
        };
        let connection = self.host.borrow().connection.clone();
        self.host
            .borrow_mut()
            .state
            .set_pointer_cursor_for(&connection, &self.surface.id(), edge);
    }
    pub(crate) fn display_ptr(&self) -> *mut c_void {
        self.host.borrow().connection.backend().display_ptr().cast()
    }
    pub(crate) fn surface_ptr(&self) -> *mut c_void {
        self.surface.id().as_ptr().cast()
    }
}

pub(crate) fn connect_to_env() -> Result<Connection, GfxInitError> {
    if env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(GfxInitError::Environment(
            "WAYLAND_DISPLAY is unset; run Leyline inside a GNOME Wayland session".into(),
        ));
    }
    Connection::connect_to_env().map_err(|error| {
        GfxInitError::Environment(format!(
            "cannot connect to the Wayland compositor: {error}; verify WAYLAND_DISPLAY and socket access"
        ))
    })
}

#[allow(clippy::struct_excessive_bools)]
pub(crate) struct WaylandState {
    compositor: CompositorState,
    shell: XdgShell,
    outputs: OutputState,
    seats: SeatState,
    shm: Shm,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keyboard_focus: Option<ObjectId>,
    pointer: Option<ThemedPointer>,
    pointer_seat: Option<wl_seat::WlSeat>,
    cursor_surface: wl_surface::WlSurface,
    pointer_icon: Option<CursorIcon>,
    pointer_focus: Option<ObjectId>,
    data_device_manager: Option<DataDeviceManagerState>,
    data_device: Option<DataDevice>,
    clipboard_offer: Option<SelectionOffer>,
    clipboard_source: Option<(u64, CopyPasteSource)>,
    primary_selection_manager: Option<PrimarySelectionManagerState>,
    primary_device: Option<PrimarySelectionDevice>,
    primary_offer: Option<PrimarySelectionOffer>,
    primary_source: Option<(u64, PrimarySelectionSource)>,
    text_input_manager: Option<ZwpTextInputManagerV3>,
    text_input: Option<ZwpTextInputV3>,
    text_input_focus: Option<ObjectId>,
    text_input_commits: u32,
    modifiers: ModifiersState,
    pressed_modifiers: PressedModifiers,
    key_repeat: KeyRepeatState,
    shortcut_digit_rows: HashMap<u32, std::num::NonZeroU8>,
    keymap_generation: u64,
    xkb_keymap: Option<xkb::Keymap>,
    xkb_state: Option<xkb::State>,
    windows: HashMap<ObjectId, Rc<RefCell<WindowEventState>>>,
    fractional_windows: HashMap<ObjectId, ObjectId>,
    seat_token: SeatToken,
}

#[allow(clippy::struct_excessive_bools)]
struct WindowEventState {
    keyboard_focused: bool,
    libdecor_resize_fallback: bool,
    pointer_cursor: PointerCursor,
    pointer_resize_edge: Option<ResizeEdge>,
    resize_press_active: bool,
    text_input_focused: bool,
    logical_size: LogicalSize,
    scale: Scale120,
    pending: PendingEvents,
    configured: bool,
    suspended: bool,
}

impl WindowEventState {
    fn new(logical_size: LogicalSize) -> Self {
        Self {
            keyboard_focused: false,
            libdecor_resize_fallback: false,
            pointer_cursor: PointerCursor::Text,
            pointer_resize_edge: None,
            resize_press_active: false,
            text_input_focused: false,
            logical_size,
            scale: Scale120::ONE,
            pending: PendingEvents::default(),
            configured: false,
            suspended: false,
        }
    }
}

fn text_input_request_owned_by<T: PartialEq>(
    locally_focused: bool,
    focus: Option<&T>,
    surface: &T,
) -> bool {
    locally_focused && focus == Some(surface)
}

fn leave_text_input_focus<T: PartialEq>(focus: &mut Option<T>, surface: &T) {
    if focus.as_ref() == Some(surface) {
        *focus = None;
    }
}

impl WaylandState {
    fn window_for_surface(
        &self,
        surface: &wl_surface::WlSurface,
    ) -> Option<Rc<RefCell<WindowEventState>>> {
        self.windows.get(&surface.id()).cloned()
    }

    fn focused_window(&self) -> Option<Rc<RefCell<WindowEventState>>> {
        self.keyboard_focus
            .as_ref()
            .and_then(|id| self.windows.get(id))
            .cloned()
            .or_else(|| self.windows.values().next().cloned())
    }

    fn push_focused(&self, event: PlatformEvent) {
        if let Some(window) = self.focused_window() {
            window.borrow_mut().pending.push_input(event);
        }
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for WaylandState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.ensure_selection_devices(qh, &seat);
    }
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        // SeatState::new binds seats already present in the initial registry without
        // invoking new_seat, so the first capability event is the reliable setup point.
        self.ensure_selection_devices(qh, &seat);
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seats.get_keyboard(qh, &seat, None) {
                Ok(keyboard) => {
                    tracing::debug!("Wayland keyboard capability initialized");
                    self.keyboard = Some(keyboard);
                }
                Err(error) => {
                    tracing::error!(%error, "failed to initialize required Wayland keyboard");
                }
            }
            if self.text_input.is_none()
                && let Some(manager) = self.text_input_manager.as_ref()
            {
                self.text_input = Some(manager.get_text_input(&seat, qh, ()));
            }
        } else if capability == Capability::Pointer && self.pointer.is_none() {
            let cursor_surface = self
                .seats
                .get_pointer_with_theme(
                    qh,
                    &seat,
                    self.shm.wl_shm(),
                    self.cursor_surface.clone(),
                    ThemeSpec::System,
                )
                .ok();
            if cursor_surface.is_some() {
                self.pointer_seat = Some(seat);
                self.pointer = cursor_surface;
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
            if let Some(text_input) = self.text_input.take() {
                text_input.destroy();
            }
            self.text_input_focus = None;
            self.text_input_commits = 0;
            self.pressed_modifiers = PressedModifiers::default();
            self.key_repeat.cancel();
        }
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            drop(pointer);
            self.pointer_seat = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self
            .data_device
            .as_ref()
            .is_some_and(|device| device.data().seat() == &seat)
        {
            self.data_device = None;
            self.clipboard_offer = None;
            self.primary_device = None;
            self.primary_offer = None;
            self.clipboard_source = None;
            self.primary_source = None;
            self.push_focused(PlatformEvent::Clipboard(ClipboardEvent::Unavailable(
                SelectionTarget::Clipboard,
            )));
            self.push_focused(PlatformEvent::Clipboard(ClipboardEvent::Unavailable(
                SelectionTarget::Primary,
            )));
            self.seat_token = self.seat_token.next_generation();
        }
    }
}

impl KeyboardHandler for WaylandState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if let Some(window) = self.window_for_surface(surface) {
            self.keyboard_focus = Some(surface.id());
            window.borrow_mut().keyboard_focused = true;
            tracing::debug!("Wayland keyboard focus entered terminal surface");
            window
                .borrow_mut()
                .pending
                .push_input(PlatformEvent::KeyboardFocus {
                    seat: self.seat_token,
                    serial: InputSerial {
                        seat: self.seat_token,
                        value: serial,
                        kind: SerialKind::Keyboard,
                    },
                    focused: true,
                });
        }
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
    ) {
        if let Some(window) = self.window_for_surface(surface) {
            if self.keyboard_focus.as_ref() == Some(&surface.id()) {
                self.keyboard_focus = None;
            }
            window.borrow_mut().keyboard_focused = false;
            self.pressed_modifiers = PressedModifiers::default();
            self.key_repeat.cancel();
            window
                .borrow_mut()
                .pending
                .push_input(PlatformEvent::KeyboardFocus {
                    seat: self.seat_token,
                    serial: InputSerial {
                        seat: self.seat_token,
                        value: serial,
                        kind: SerialKind::Keyboard,
                    },
                    focused: false,
                });
        }
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        if self.keyboard_focus.is_none() {
            return;
        }
        self.push_key(serial, event.clone(), KeyState::Pressed, false);
        self.key_repeat.press(serial, event, Instant::now());
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        if self.keyboard_focus.is_none() {
            return;
        }
        self.key_repeat.cancel();
        self.push_key(serial, event, KeyState::Pressed, true);
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        if self.keyboard_focus.is_none() {
            return;
        }
        self.key_repeat.release(event.raw_code);
        self.push_key(serial, event, KeyState::Released, false);
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        raw: RawModifiers,
        layout: u32,
    ) {
        self.modifiers = ModifiersState {
            shift: modifiers.shift,
            control: modifiers.ctrl,
            alt: modifiers.alt,
            super_key: modifiers.logo,
            alt_graph: false,
            caps_lock: modifiers.caps_lock,
            num_lock: modifiers.num_lock,
        };
        if let Some(state) = self.xkb_state.as_mut() {
            state.update_mask(raw.depressed, raw.latched, raw.locked, 0, 0, layout);
        }
        if self.keyboard_focus.is_some() {
            self.push_focused(PlatformEvent::ModifiersChanged(self.modifiers));
        }
    }

    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        keymap: Keymap<'_>,
    ) {
        let keymap_text = keymap.as_string();
        self.shortcut_digit_rows = parse_digit_row_keycodes(&keymap_text);
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        self.xkb_keymap = xkb::Keymap::new_from_string(
            &context,
            keymap_text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        );
        self.xkb_state = self.xkb_keymap.as_ref().map(xkb::State::new);
        self.keymap_generation = self.keymap_generation.wrapping_add(1);
        tracing::debug!("Wayland keyboard keymap initialized");
    }

    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        info: RepeatInfo,
    ) {
        self.key_repeat.update_info(info);
    }
}

fn parse_digit_row_keycodes(keymap: &str) -> HashMap<u32, std::num::NonZeroU8> {
    let mut result = HashMap::new();
    for line in keymap.lines() {
        for digit in 1_u8..=9 {
            let name = format!("<AE0{digit}>");
            let Some(rest) = line.trim().strip_prefix(&name) else {
                continue;
            };
            let Some(code) = rest
                .trim_start()
                .strip_prefix('=')
                .and_then(|value| value.trim().trim_end_matches(';').parse::<u32>().ok())
            else {
                continue;
            };
            if let (Some(raw), Some(digit)) = (code.checked_sub(8), std::num::NonZeroU8::new(digit))
            {
                result.insert(raw, digit);
            }
        }
    }
    result
}

impl WaylandState {
    fn emit_due_key_repeat(&mut self, now: Instant) {
        if self.keyboard_focus.is_some()
            && let Some((serial, event)) = self.key_repeat.take_due(now)
        {
            self.push_key(serial, event, KeyState::Pressed, true);
        }
    }

    fn ensure_selection_devices(&mut self, qh: &QueueHandle<Self>, seat: &wl_seat::WlSeat) {
        if self.data_device.is_none()
            && let Some(manager) = self.data_device_manager.as_ref()
        {
            self.data_device = Some(manager.get_data_device(qh, seat));
            tracing::debug!("Wayland clipboard data device initialized");
        }
        if self.primary_device.is_none()
            && let Some(manager) = self.primary_selection_manager.as_ref()
        {
            self.primary_device = Some(manager.get_selection_device(qh, seat));
            tracing::debug!("Wayland primary-selection device initialized");
        }
    }

    fn push_key(&mut self, serial: u32, event: KeyEvent, state: KeyState, repeat: bool) {
        self.pressed_modifiers.update(event.keysym.raw(), state);
        let modifiers = self.pressed_modifiers.merge(self.modifiers);
        tracing::trace!(
            physical_keycode = event.raw_code,
            keysym = event.keysym.raw(),
            has_utf8 = event.utf8.as_ref().is_some_and(|text| !text.is_empty()),
            ?state,
            repeat,
            shift = modifiers.shift,
            control = modifiers.control,
            alt = modifiers.alt,
            super_key = modifiers.super_key,
            "Wayland keyboard event"
        );
        let identity = self.key_identity(&event);
        self.push_focused(PlatformEvent::Key(KeyInput {
            serial: InputSerial {
                seat: self.seat_token,
                value: serial,
                kind: SerialKind::Keyboard,
            },
            time_ms: event.time,
            physical_keycode: event.raw_code,
            shortcut_digit_row: self.shortcut_digit_rows.get(&event.raw_code).copied(),
            utf8: event.utf8,
            modifiers,
            shortcut_modifiers: shortcut_modifiers(modifiers, self.pressed_modifiers),
            logical_key: identity.logical,
            identity,
            keymap_generation: self.keymap_generation,
            state,
            repeat,
        }));
    }

    fn key_identity(&self, event: &KeyEvent) -> crate::KeyIdentity {
        let mut identity = key_identity_from_keysym(event.keysym.raw());
        let Some(raw_code) = event.raw_code.checked_add(8) else {
            return identity;
        };
        let keycode = xkb::Keycode::new(raw_code);
        let Some(keymap) = self.xkb_keymap.as_ref() else {
            return identity;
        };
        if let Some(name) = keymap.key_get_name(keycode)
            && let Some(keypad) = keypad_key_from_xkb_name(name)
        {
            identity.location = crate::KeyLocation::Numpad;
            identity.keypad = Some(keypad);
        }
        let layout = self
            .xkb_state
            .as_ref()
            .map_or(0, |state| state.key_get_layout(keycode));
        identity.base_codepoint = keymap
            .key_get_syms_by_level(keycode, 0, 0)
            .first()
            .and_then(|keysym| crate::keysym_character(keysym.raw()));
        identity.shifted_codepoint = keymap
            .key_get_syms_by_level(keycode, layout, 1)
            .first()
            .and_then(|keysym| crate::keysym_character(keysym.raw()));
        identity
    }

    fn bump_text_input_commit(&mut self) -> Option<u32> {
        self.text_input_commits = self.text_input_commits.checked_add(1)?;
        Some(self.text_input_commits)
    }
}

fn keypad_key_from_xkb_name(name: &str) -> Option<crate::KeypadKey> {
    use crate::KeypadKey;
    Some(match name {
        "KP0" => KeypadKey::Digit(0),
        "KP1" => KeypadKey::Digit(1),
        "KP2" => KeypadKey::Digit(2),
        "KP3" => KeypadKey::Digit(3),
        "KP4" => KeypadKey::Digit(4),
        "KP5" => KeypadKey::Digit(5),
        "KP6" => KeypadKey::Digit(6),
        "KP7" => KeypadKey::Digit(7),
        "KP8" => KeypadKey::Digit(8),
        "KP9" => KeypadKey::Digit(9),
        "KPDL" => KeypadKey::Decimal,
        "KPCO" | "KPSP" => KeypadKey::Separator,
        "KPAD" => KeypadKey::Add,
        "KPSU" => KeypadKey::Subtract,
        "KPMU" => KeypadKey::Multiply,
        "KPDV" => KeypadKey::Divide,
        "KPEQ" => KeypadKey::Equal,
        "KPEN" => KeypadKey::Enter,
        _ => return None,
    })
}

impl PointerHandler for WaylandState {
    fn pointer_frame(
        &mut self,
        connection: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(window) = self.window_for_surface(&event.surface) else {
                continue;
            };
            let mut local = window.borrow_mut();
            let edge = local
                .libdecor_resize_fallback
                .then(|| content_resize_edge(event.position, local.logical_size))
                .flatten();
            local.pointer_resize_edge = edge;
            let kind = match &event.kind {
                PointerEventKind::Enter { serial } => {
                    // Cursor requests are serial-bound and must be repeated for every enter.
                    self.pointer_icon = None;
                    self.pointer_focus = Some(event.surface.id());
                    drop(local);
                    self.set_pointer_cursor_for(connection, &event.surface.id(), edge);
                    local = window.borrow_mut();
                    PointerKind::Enter {
                        serial: self.pointer_serial(*serial),
                    }
                }
                PointerEventKind::Leave { serial } => {
                    local.pointer_resize_edge = None;
                    if self.pointer_focus.as_ref() == Some(&event.surface.id()) {
                        self.pointer_focus = None;
                    }
                    PointerKind::Leave {
                        serial: self.pointer_serial(*serial),
                    }
                }
                PointerEventKind::Motion { time } => {
                    drop(local);
                    self.set_pointer_cursor_for(connection, &event.surface.id(), edge);
                    local = window.borrow_mut();
                    PointerKind::Motion { time_ms: *time }
                }
                PointerEventKind::Press {
                    time,
                    button,
                    serial,
                } => {
                    local.resize_press_active = false;
                    if *button == 0x110
                        && let (Some(edge), Some(seat)) = (edge, self.pointer_seat.clone())
                    {
                        local.pending.resize = Some((seat, *serial, edge));
                        local.resize_press_active = true;
                        continue;
                    }
                    PointerKind::Press {
                        serial: self.pointer_serial(*serial),
                        time_ms: *time,
                        button: *button,
                    }
                }
                PointerEventKind::Release {
                    time,
                    button,
                    serial,
                } => {
                    if *button == 0x110 && std::mem::take(&mut local.resize_press_active) {
                        continue;
                    }
                    PointerKind::Release {
                        serial: self.pointer_serial(*serial),
                        time_ms: *time,
                        button: *button,
                    }
                }
                PointerEventKind::Axis {
                    time,
                    horizontal,
                    vertical,
                    ..
                } => PointerKind::Axis {
                    time_ms: *time,
                    horizontal_120: horizontal.value120,
                    vertical_120: vertical.value120,
                },
            };
            local
                .pending
                .push_input(PlatformEvent::Pointer(PointerInput {
                    position: event.position,
                    kind,
                }));
        }
    }
}

impl WaylandState {
    const fn pointer_serial(&self, value: u32) -> InputSerial {
        InputSerial {
            seat: self.seat_token,
            value,
            kind: SerialKind::Pointer,
        }
    }

    fn set_pointer_cursor_for(
        &mut self,
        connection: &Connection,
        surface: &ObjectId,
        edge: Option<ResizeEdge>,
    ) {
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        let cursor = self
            .windows
            .get(surface)
            .map_or(PointerCursor::Text, |window| window.borrow().pointer_cursor);
        let icon = match edge {
            Some(ResizeEdge::Top) => CursorIcon::NResize,
            Some(ResizeEdge::Bottom) => CursorIcon::SResize,
            Some(ResizeEdge::Left) => CursorIcon::WResize,
            Some(ResizeEdge::TopLeft) => CursorIcon::NwResize,
            Some(ResizeEdge::BottomLeft) => CursorIcon::SwResize,
            Some(ResizeEdge::Right) => CursorIcon::EResize,
            Some(ResizeEdge::TopRight) => CursorIcon::NeResize,
            Some(ResizeEdge::BottomRight) => CursorIcon::SeResize,
            None => match cursor {
                PointerCursor::Text => CursorIcon::Text,
                PointerCursor::Grab => CursorIcon::Grab,
                PointerCursor::Grabbing => CursorIcon::Grabbing,
            },
        };
        if self.pointer_icon == Some(icon) {
            return;
        }
        if let Err(error) = pointer.set_cursor(connection, icon) {
            tracing::trace!(%error, "could not update terminal pointer cursor");
        } else {
            self.pointer_icon = Some(icon);
        }
    }
}

fn content_resize_edge(position: (f64, f64), size: LogicalSize) -> Option<ResizeEdge> {
    let (x, y) = position;
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return None;
    }
    let right = x >= (f64::from(size.width) - CONTENT_RESIZE_MARGIN).max(0.0);
    let bottom = y >= (f64::from(size.height) - CONTENT_RESIZE_MARGIN).max(0.0);
    let left = x < CONTENT_RESIZE_MARGIN;
    let top = y < CONTENT_RESIZE_MARGIN;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(ResizeEdge::TopLeft),
        (true, _, _, true) => Some(ResizeEdge::BottomLeft),
        (_, true, true, _) => Some(ResizeEdge::TopRight),
        (_, true, _, true) => Some(ResizeEdge::BottomRight),
        (true, _, _, _) => Some(ResizeEdge::Left),
        (_, true, _, _) => Some(ResizeEdge::Right),
        (_, _, true, _) => Some(ResizeEdge::Top),
        (_, _, _, true) => Some(ResizeEdge::Bottom),
        _ => None,
    }
}

#[cfg(test)]
mod resize_tests {
    use super::content_resize_edge;
    use crate::{LogicalSize, decor::ResizeEdge};

    const SIZE: LogicalSize = LogicalSize {
        width: 800,
        height: 500,
    };

    #[test]
    fn content_resize_margin_covers_edges_and_corners() {
        assert!(matches!(
            content_resize_edge((0.0, 0.0), SIZE),
            Some(ResizeEdge::TopLeft)
        ));
        assert!(matches!(
            content_resize_edge((799.0, 499.0), SIZE),
            Some(ResizeEdge::BottomRight)
        ));
        assert!(matches!(
            content_resize_edge((400.0, 1.0), SIZE),
            Some(ResizeEdge::Top)
        ));
        assert!(content_resize_edge((400.0, 250.0), SIZE).is_none());
    }

    #[test]
    fn content_resize_margin_rejects_invalid_coordinates() {
        assert!(content_resize_edge((-1.0, 10.0), SIZE).is_none());
        assert!(content_resize_edge((f64::NAN, 10.0), SIZE).is_none());
    }
}

#[cfg(test)]
mod modifier_tests {
    use super::{PressedModifiers, shortcut_modifiers};
    use crate::{KeyState, ModifierMask, ModifiersState};

    #[test]
    fn physical_shift_bridges_late_compositor_modifier_updates() {
        let mut pressed = PressedModifiers::default();
        pressed.update(0xffe1, KeyState::Pressed);
        assert!(pressed.merge(ModifiersState::default()).shift);

        pressed.update(0xffe2, KeyState::Pressed);
        pressed.update(0xffe1, KeyState::Released);
        assert!(pressed.merge(ModifiersState::default()).shift);

        pressed.update(0xffe2, KeyState::Released);
        assert!(!pressed.merge(ModifiersState::default()).shift);
    }

    #[test]
    fn compositor_modifier_state_remains_authoritative() {
        let pressed = PressedModifiers::default();
        let reported = ModifiersState {
            control: true,
            ..ModifiersState::default()
        };
        assert!(pressed.merge(reported).control);
    }

    #[test]
    fn alt_graph_does_not_impersonate_control_alt_shortcut() {
        let mut pressed = PressedModifiers::default();
        pressed.update(0xfe03, KeyState::Pressed);
        let effective = pressed.merge(ModifiersState {
            control: true,
            alt: true,
            ..ModifiersState::default()
        });
        assert!(effective.alt_graph);
        assert_eq!(
            shortcut_modifiers(effective, pressed),
            ModifierMask::empty()
        );

        pressed.update(0xffe3, KeyState::Pressed);
        pressed.update(0xffe9, KeyState::Pressed);
        let shortcut = shortcut_modifiers(effective, pressed);
        assert!(shortcut.contains(ModifierMask::CONTROL));
        assert!(shortcut.contains(ModifierMask::ALT));
    }
}

#[cfg(test)]
mod key_repeat_tests {
    use std::{num::NonZeroU32, time::Duration};

    use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, RepeatInfo};

    use super::{KeyRepeatState, keypad_key_from_xkb_name, parse_digit_row_keycodes};

    fn key(raw_code: u32, keysym: u32) -> KeyEvent {
        KeyEvent {
            time: 10,
            raw_code,
            keysym: Keysym::new(keysym),
            utf8: Some("a".into()),
        }
    }

    #[test]
    fn repeat_obeys_compositor_delay_rate_and_release() {
        let mut repeat = KeyRepeatState::default();
        repeat.update_info(RepeatInfo::Repeat {
            rate: NonZeroU32::new(20).unwrap(),
            delay: 300,
        });
        let start = std::time::Instant::now();
        repeat.press(7, key(30, 0x61), start);

        assert!(
            repeat
                .take_due(start + Duration::from_millis(299))
                .is_none()
        );
        let (serial, event) = repeat.take_due(start + Duration::from_millis(300)).unwrap();
        assert_eq!(serial, 7);
        assert_eq!(event.raw_code, 30);
        assert!(
            repeat
                .take_due(start + Duration::from_millis(349))
                .is_none()
        );
        assert!(
            repeat
                .take_due(start + Duration::from_millis(350))
                .is_some()
        );

        repeat.release(30);
        assert!(repeat.deadline().is_none());
    }

    #[test]
    fn unidentified_modifier_does_not_repeat() {
        let mut repeat = KeyRepeatState::default();
        repeat.update_info(RepeatInfo::Repeat {
            rate: NonZeroU32::new(20).unwrap(),
            delay: 300,
        });
        repeat.press(7, key(42, 0xffe1), std::time::Instant::now());
        assert!(repeat.deadline().is_none());
    }

    #[test]
    fn xkb_key_names_normalize_digit_row_independently_of_symbols() {
        let parsed = parse_digit_row_keycodes("xkb_keycodes {\n <AE01> = 10;\n <AE09> = 18;\n};");
        assert_eq!(parsed.get(&2).map(|value| value.get()), Some(1));
        assert_eq!(parsed.get(&10).map(|value| value.get()), Some(9));
    }

    #[test]
    fn xkb_key_names_keep_keypad_identity_independent_of_numlock_level() {
        assert_eq!(
            keypad_key_from_xkb_name("KP7"),
            Some(crate::KeypadKey::Digit(7))
        );
        assert_eq!(
            keypad_key_from_xkb_name("KPEN"),
            Some(crate::KeypadKey::Enter)
        );
        assert_eq!(keypad_key_from_xkb_name("AE07"), None);
    }
}

impl WindowHandler for WaylandState {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        if let Some(local) = self.window_for_surface(window.wl_surface()) {
            local.borrow_mut().pending.close = true;
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let Some(local) = self.window_for_surface(window.wl_surface()) else {
            return;
        };
        let mut local = local.borrow_mut();
        if let Some(width) = configure.new_size.0 {
            local.logical_size.width = width.get();
        }
        if let Some(height) = configure.new_size.1 {
            local.logical_size.height = height.get();
        }
        local.configured = true;
        let suspended = configure.state.contains(XdgWindowState::SUSPENDED);
        if suspended != local.suspended {
            local.suspended = suspended;
            local.pending.suspended = Some(suspended);
        }
        let logical_size = local.logical_size;
        let scale = local.scale;
        local.pending.configured = Some((
            logical_size,
            scale,
            WindowState {
                maximized: configure.is_maximized(),
                fullscreen: configure.is_fullscreen(),
                activated: configure.is_activated(),
            },
        ));
    }
}

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        scale: i32,
    ) {
        if let Some(local) = self.window_for_surface(surface) {
            let mut local = local.borrow_mut();
            local.scale = Scale120(u32::try_from(scale.max(1)).unwrap_or(1).saturating_mul(120));
            local.pending.scale = Some(local.scale);
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(local) = self.window_for_surface(surface) {
            local.borrow_mut().pending.frame_ready = true;
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let scale = Scale120(scale.max(1));
            if let Some(local) = state
                .fractional_windows
                .get(&proxy.id())
                .and_then(|id| state.windows.get(id))
                .cloned()
            {
                let mut local = local.borrow_mut();
                if local.scale != scale {
                    local.scale = scale;
                    local.pending.scale = Some(scale);
                }
            }
        }
    }
}

impl Dispatch<WpViewporter, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: wp_viewport::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &ZwpTextInputManagerV3,
        _: <ZwpTextInputManagerV3 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, ()> for WaylandState {
    #[allow(clippy::too_many_lines)]
    fn event(
        state: &mut Self,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let event = match event {
            zwp_text_input_v3::Event::Enter { surface }
                if state.windows.contains_key(&surface.id()) =>
            {
                tracing::debug!(
                    category = "text_input_event",
                    event = "enter",
                    "text-input focus entered surface"
                );
                state.text_input_focus = Some(surface.id());
                state
                    .windows
                    .get(&surface.id())
                    .unwrap()
                    .borrow_mut()
                    .text_input_focused = true;
                Some(TextInputEvent::Enter)
            }
            zwp_text_input_v3::Event::Leave { surface }
                if state.windows.contains_key(&surface.id()) =>
            {
                tracing::debug!(
                    category = "text_input_event",
                    event = "leave",
                    "text-input focus left surface"
                );
                if let Some(local) = state.windows.get(&surface.id()) {
                    let mut local = local.borrow_mut();
                    local.text_input_focused = false;
                    local
                        .pending
                        .push_input(PlatformEvent::TextInput(TextInputEvent::Leave));
                }
                leave_text_input_focus(&mut state.text_input_focus, &surface.id());
                None
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } if state.text_input_focus.is_some() => {
                let text = text.unwrap_or_default();
                tracing::trace!(
                    category = "text_input_event",
                    event = "preedit",
                    bytes = text.len(),
                    has_cursor = cursor_begin != -1 || cursor_end != -1,
                    "received text-input event"
                );
                Some(TextInputEvent::Preedit {
                    text,
                    cursor: (cursor_begin != -1 || cursor_end != -1)
                        .then_some((cursor_begin, cursor_end)),
                })
            }
            zwp_text_input_v3::Event::CommitString { text } if state.text_input_focus.is_some() => {
                let text = text.unwrap_or_default();
                tracing::trace!(
                    category = "text_input_event",
                    event = "commit",
                    bytes = text.len(),
                    "received text-input event"
                );
                Some(TextInputEvent::Commit(text))
            }
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } if state.text_input_focus.is_some() => {
                tracing::trace!(
                    category = "text_input_event",
                    event = "delete_surrounding",
                    before_bytes = before_length,
                    after_bytes = after_length,
                    "received text-input event"
                );
                Some(TextInputEvent::DeleteSurrounding {
                    before_bytes: before_length,
                    after_bytes: after_length,
                })
            }
            zwp_text_input_v3::Event::Done { serial } if state.text_input_focus.is_some() => {
                tracing::trace!(
                    category = "text_input_event",
                    event = "done",
                    serial,
                    expected_serial = state.text_input_commits,
                    "received text-input event"
                );
                Some(TextInputEvent::Done { serial })
            }
            _ => None,
        };
        if let Some(event) = event
            && let Some(id) = state.text_input_focus.as_ref()
            && let Some(local) = state.windows.get(id)
        {
            local
                .borrow_mut()
                .pending
                .push_input(PlatformEvent::TextInput(event));
        }
    }
}

impl DataDeviceHandler for WaylandState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
        _: f64,
        _: f64,
        _: &wl_surface::WlSurface,
    ) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {}
    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
        _: f64,
        _: f64,
    ) {
    }
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &wl_data_device::WlDataDevice,
    ) {
        let Some(data_device) = self
            .data_device
            .as_ref()
            .filter(|value| value.inner() == device)
        else {
            return;
        };
        self.clipboard_offer = data_device.data().selection_offer();
        let event = self.clipboard_offer.as_ref().map_or(
            ClipboardEvent::Cleared(SelectionTarget::Clipboard),
            |offer| ClipboardEvent::Offer {
                target: SelectionTarget::Clipboard,
                mime_types: offer.with_mime_types(<[String]>::to_vec),
            },
        );
        self.push_focused(PlatformEvent::Clipboard(event));
    }
    fn drop_performed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_device::WlDataDevice,
    ) {
    }
}

impl DataOfferHandler for WaylandState {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl DataSourceHandler for WaylandState {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_source::WlDataSource,
        _: Option<String>,
    ) {
    }
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &wl_data_source::WlDataSource,
        mime_type: String,
        fd: smithay_client_toolkit::data_device_manager::WritePipe,
    ) {
        if let Some((source, _)) = self
            .clipboard_source
            .as_ref()
            .filter(|(_, value)| value.inner() == source)
        {
            self.push_focused(PlatformEvent::Clipboard(ClipboardEvent::Send {
                target: SelectionTarget::Clipboard,
                source: *source,
                mime_type,
                fd: OwnedFd::from(fd),
            }));
        }
    }
    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &wl_data_source::WlDataSource,
    ) {
        if self
            .clipboard_source
            .as_ref()
            .is_some_and(|(_, value)| value.inner() == source)
        {
            let (source, _) = self.clipboard_source.take().expect("source was checked");
            self.push_focused(PlatformEvent::Clipboard(ClipboardEvent::SourceCancelled {
                target: SelectionTarget::Clipboard,
                source,
            }));
        }
    }
    fn dnd_dropped(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_source::WlDataSource,
    ) {
    }
    fn dnd_finished(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_source::WlDataSource,
    ) {
    }
    fn action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_data_source::WlDataSource,
        _: DndAction,
    ) {
    }
}

impl PrimarySelectionDeviceHandler for WaylandState {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    ) {
        let Some(primary_device) = self
            .primary_device
            .as_ref()
            .filter(|value| value.inner() == device)
        else {
            return;
        };
        self.primary_offer = primary_device.data().selection_offer();
        let event = self.primary_offer.as_ref().map_or(
            ClipboardEvent::Cleared(SelectionTarget::Primary),
            |offer| ClipboardEvent::Offer {
                target: SelectionTarget::Primary,
                mime_types: offer.with_mime_types(<[String]>::to_vec),
            },
        );
        self.push_focused(PlatformEvent::Clipboard(event));
    }
}

impl PrimarySelectionSourceHandler for WaylandState {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
        mime_type: String,
        fd: smithay_client_toolkit::data_device_manager::WritePipe,
    ) {
        if let Some((source, _)) = self
            .primary_source
            .as_ref()
            .filter(|(_, value)| value.inner() == source)
        {
            self.push_focused(PlatformEvent::Clipboard(ClipboardEvent::Send {
                target: SelectionTarget::Primary,
                source: *source,
                mime_type,
                fd: OwnedFd::from(fd),
            }));
        }
    }
    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
    ) {
        if self
            .primary_source
            .as_ref()
            .is_some_and(|(_, value)| value.inner() == source)
        {
            let (source, _) = self.primary_source.take().expect("source was checked");
            self.push_focused(PlatformEvent::Clipboard(ClipboardEvent::SourceCancelled {
                target: SelectionTarget::Primary,
                source,
            }));
        }
    }
}

delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_seat!(WaylandState);
delegate_keyboard!(WaylandState);
delegate_pointer!(WaylandState);
delegate_shm!(WaylandState);
delegate_data_device!(WaylandState);
delegate_primary_selection!(WaylandState);
delegate_xdg_shell!(WaylandState);
delegate_xdg_window!(WaylandState);

#[cfg(test)]
mod retained_event_tests {
    use super::*;

    fn motion(time_ms: u32) -> PlatformEvent {
        PlatformEvent::Pointer(PointerInput {
            position: (f64::from(time_ms), 0.0),
            kind: PointerKind::Motion { time_ms },
        })
    }

    #[test]
    fn retained_input_is_bounded_and_drained_by_round_budget() {
        let mut pending = PendingEvents::default();
        for _ in 0..=RETAINED_INPUT_CAPACITY {
            pending.push_input(PlatformEvent::FrameReady);
        }
        assert_eq!(pending.input.len(), RETAINED_INPUT_CAPACITY);
        assert!(pending.input_overflowed);
        assert!(!pending.can_dispatch_callbacks());

        let mut first_round = Vec::new();
        pending.drain_input(&mut first_round);
        assert_eq!(first_round.len(), INPUT_DRAIN_BUDGET);
        assert_eq!(
            pending.input.len(),
            RETAINED_INPUT_CAPACITY - INPUT_DRAIN_BUDGET
        );
        assert!(!pending.can_dispatch_callbacks());

        for _ in 1..(RETAINED_INPUT_CAPACITY / INPUT_DRAIN_BUDGET) {
            pending.drain_input(&mut Vec::new());
        }
        assert!(pending.can_dispatch_callbacks());
    }

    #[test]
    fn consecutive_pointer_motion_is_coalesced_without_crossing_ordered_events() {
        let mut pending = PendingEvents::default();
        for time_ms in 0..1_000 {
            pending.push_input(motion(time_ms));
        }
        assert_eq!(pending.input.len(), 1);
        assert!(!pending.input_overflowed);

        pending.push_input(PlatformEvent::ModifiersChanged(ModifiersState::default()));
        pending.push_input(motion(1_000));
        assert_eq!(pending.input.len(), 3);

        let PlatformEvent::Pointer(pointer) = pending.input.front().unwrap() else {
            panic!("expected coalesced pointer motion");
        };
        assert_eq!(pointer.position, (999.0, 0.0));
    }

    #[test]
    fn stale_surface_cannot_modify_new_text_input_owner() {
        let old = 1_u8;
        let new = 2_u8;
        let mut focus = Some(old);

        assert!(text_input_request_owned_by(true, focus.as_ref(), &old));

        focus = Some(new);
        leave_text_input_focus(&mut focus, &old);
        assert!(!text_input_request_owned_by(true, focus.as_ref(), &old));
        assert!(text_input_request_owned_by(true, focus.as_ref(), &new));
        assert!(!text_input_request_owned_by(false, focus.as_ref(), &new));
    }
}

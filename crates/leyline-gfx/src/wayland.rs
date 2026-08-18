use std::{
    collections::{HashMap, VecDeque},
    env,
    ffi::c_void,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
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
    backend::WaylandError,
    globals::{GlobalListContents, registry_queue_init},
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

use crate::decor::{Libdecor, ResizeEdge};
use crate::{
    ClipboardEvent, GfxInitError, InputSerial, KeyInput, KeyState, LogicalSize, ModifierMask,
    ModifiersState, PlatformEvent, PointerCursor, PointerInput, PointerKind, Scale120, SeatToken,
    SelectionTarget, SerialKind, TextInputContext, TextInputEvent, TextInputPurpose, WindowState,
    logical_key_from_keysym,
};

const DEFAULT_SIZE: LogicalSize = LogicalSize {
    width: 800,
    height: 500,
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
            crate::LogicalKey::Unidentified(_)
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

pub(crate) struct WaylandWindow {
    pub(crate) connection: Connection,
    event_queue: EventQueue<WaylandState>,
    pub(crate) state: WaylandState,
    window: Option<Window>,
    surface: wl_surface::WlSurface,
    libdecor: Option<Libdecor>,
    _fractional_scale: Option<WpFractionalScaleV1>,
    viewport: Option<WpViewport>,
    flush_blocked: bool,
}

impl Drop for WaylandWindow {
    fn drop(&mut self) {
        // libdecor keeps raw references to the display and surface, so release it before Rust
        // drops the Wayland objects declared earlier in this struct.
        drop(self.libdecor.take());
    }
}

impl WaylandWindow {
    #[allow(clippy::too_many_lines)]
    pub(crate) fn connect(title: &str) -> Result<Self, GfxInitError> {
        if env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err(GfxInitError::Environment(
                "WAYLAND_DISPLAY is unset; run Leyline inside a GNOME Wayland session".into(),
            ));
        }
        let connection = Connection::connect_to_env().map_err(|error| {
            GfxInitError::Environment(format!(
                "cannot connect to the Wayland compositor: {error}; verify WAYLAND_DISPLAY and socket access"
            ))
        })?;
        let (globals, event_queue) = registry_queue_init(&connection).map_err(|error| {
            GfxInitError::Platform(format!("cannot read Wayland globals: {error}"))
        })?;
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
        let surface = compositor.create_surface(&qh);
        let cursor_surface = compositor.create_surface(&qh);
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
        let text_input_manager = globals
            .bind::<ZwpTextInputManagerV3, _, _>(&qh, 1..=2, ())
            .ok();
        let data_device_manager = DataDeviceManagerState::bind(&globals, &qh).map_err(|error| {
            GfxInitError::Platform(format!(
                "cannot bind required wl_data_device_manager: {error}"
            ))
        })?;
        let primary_selection_manager = PrimarySelectionManagerState::bind(&globals, &qh).ok();
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
                    Libdecor::new(&connection, &surface, title).map_err(|error| {
                        GfxInitError::Platform(format!(
                            "cannot create required libdecor window: {error}"
                        ))
                    })?,
                ),
            )
        };
        Ok(Self {
            state: WaylandState {
                outputs: OutputState::new(&globals, &qh),
                seats: SeatState::new(&globals, &qh),
                shm,
                keyboard: None,
                pointer: None,
                pointer_seat: None,
                cursor_surface,
                libdecor_resize_fallback: !has_server_decor,
                pointer_icon: None,
                pointer_cursor: PointerCursor::Text,
                pointer_resize_edge: None,
                resize_press_active: false,
                data_device_manager,
                data_device: None,
                clipboard_offer: None,
                clipboard_source: None,
                primary_selection_manager,
                primary_device: None,
                primary_offer: None,
                primary_source: None,
                text_input_manager,
                text_input: None,
                text_input_focused: false,
                text_input_commits: 0,
                target_surface: surface.clone(),
                modifiers: ModifiersState::default(),
                pressed_modifiers: PressedModifiers::default(),
                key_repeat: KeyRepeatState::default(),
                shortcut_digit_rows: HashMap::new(),
                logical_size: DEFAULT_SIZE,
                scale: Scale120::ONE,
                pending: PendingEvents::default(),
                configured: false,
                suspended: false,
                seat_token: SeatToken::new(0, 1),
            },
            connection,
            event_queue,
            window,
            surface,
            libdecor,
            _fractional_scale: fractional_scale,
            viewport,
            flush_blocked: false,
        })
    }

    pub(crate) fn dispatch_pending(&mut self) -> Result<(), String> {
        // Drain the retained round before admitting another callback batch. Otherwise a busy
        // input source can refill the bounded queue faster than the UI consumes its old events.
        if !self.state.pending.can_dispatch_callbacks() {
            return Ok(());
        }
        if let Some(decor) = self.libdecor.as_mut() {
            decor.dispatch(0)?;
            if let Some((size, state)) = decor.take_configured() {
                self.state.logical_size = size;
                self.state.configured = true;
                self.state.pending.configured = Some((size, self.state.scale, state));
            }
            self.state.pending.close |= decor.take_close();
            if decor.take_commit_requested() {
                self.surface.commit();
            }
        }
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| format!("Wayland dispatch failed: {error}"))?;
        self.state.emit_due_key_repeat(Instant::now());
        if let Some((seat, serial, edge)) = self.state.pending.resize.take()
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
                self.state.logical_size = size;
                self.state.configured = true;
                self.state.pending.configured = Some((size, self.state.scale, state));
            }
            if decor.take_commit_requested() {
                self.surface.commit();
            }
            return Ok(());
        }
        self.event_queue
            .roundtrip(&mut self.state)
            .map(|_| ())
            .map_err(|error| format!("Wayland roundtrip failed: {error}"))
    }

    pub(crate) fn flush(&mut self) -> Result<(), String> {
        match self.connection.flush() {
            Ok(()) => self.flush_blocked = false,
            Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                self.flush_blocked = true;
            }
            Err(error) => return Err(format!("Wayland flush failed: {error}")),
        }
        Ok(())
    }

    pub(crate) fn poll_read(
        &mut self,
        wake: Option<BorrowedFd<'_>>,
        timeout: Option<Duration>,
    ) -> Result<(), String> {
        if !self.state.pending.input.is_empty() {
            return Ok(());
        }
        self.flush()?;
        let timeout = min_timeout(timeout, self.state.key_repeat.deadline());
        if self.libdecor.is_some() {
            return self.poll_libdecor(wake, timeout);
        }
        let Some(read_guard) = self.event_queue.prepare_read() else {
            self.dispatch_pending()?;
            return Ok(());
        };
        let mut wayland_interest = PollFlags::IN | PollFlags::ERR | PollFlags::HUP;
        if self.flush_blocked {
            wayland_interest |= PollFlags::OUT;
        }
        let mut descriptors = vec![PollFd::from_borrowed_fd(
            self.connection.as_fd(),
            wayland_interest,
        )];
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
        }
        if readiness.contains(PollFlags::OUT) {
            self.flush()?;
        }
        Ok(())
    }

    fn poll_libdecor(
        &mut self,
        wake: Option<BorrowedFd<'_>>,
        timeout: Option<Duration>,
    ) -> Result<(), String> {
        let Some(read_guard) = self.event_queue.prepare_read() else {
            self.dispatch_pending()?;
            return Ok(());
        };
        let mut wayland_interest = PollFlags::IN | PollFlags::ERR | PollFlags::HUP;
        if self.flush_blocked {
            wayland_interest |= PollFlags::OUT;
        }
        let mut descriptors = vec![PollFd::from_borrowed_fd(
            self.connection.as_fd(),
            wayland_interest,
        )];
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
        } else {
            // libdecor shares this wl_display, so cancel our prepared read before dispatching.
            drop(read_guard);
        }
        if readiness.contains(PollFlags::OUT) {
            self.flush()?;
        }
        self.libdecor
            .as_mut()
            .expect("libdecor backend")
            .dispatch(0)?;
        if self
            .libdecor
            .as_mut()
            .expect("libdecor backend")
            .take_commit_requested()
        {
            self.surface.commit();
        }
        Ok(())
    }

    pub(crate) fn take_events(&mut self, output: &mut Vec<PlatformEvent>) -> Result<(), String> {
        if self.state.pending.input_overflowed {
            return Err(format!(
                "Wayland retained input queue exceeded {RETAINED_INPUT_CAPACITY} events"
            ));
        }
        if self.state.pending.close {
            self.state.pending.close = false;
            output.push(PlatformEvent::CloseRequested);
        }
        if let Some((logical_size, scale, state)) = self.state.pending.configured.take() {
            output.push(PlatformEvent::Configured {
                logical_size,
                scale,
                state,
            });
        }
        if let Some(scale) = self.state.pending.scale.take() {
            output.push(PlatformEvent::ScaleChanged { scale });
        }
        if self.state.pending.frame_ready {
            self.state.pending.frame_ready = false;
            output.push(PlatformEvent::FrameReady);
        }
        if let Some(suspended) = self.state.pending.suspended.take() {
            output.push(if suspended {
                PlatformEvent::SurfaceSuspended
            } else {
                PlatformEvent::SurfaceResumed
            });
        }
        self.state.pending.drain_input(output);
        Ok(())
    }

    pub(crate) fn request_frame(&self) {
        self.surface
            .frame(&self.event_queue.handle(), self.surface.clone());
    }

    pub(crate) fn commit(&self) {
        if let Some(viewport) = self.viewport.as_ref() {
            viewport.set_destination(
                i32::try_from(self.state.logical_size.width).unwrap_or(i32::MAX),
                i32::try_from(self.state.logical_size.height).unwrap_or(i32::MAX),
            );
        } else {
            let integer_scale = (self.state.scale.0 / 120).max(1);
            self.surface
                .set_buffer_scale(i32::try_from(integer_scale).unwrap_or(i32::MAX));
        }
        self.surface.commit();
    }

    pub(crate) const fn text_input_available(&self) -> bool {
        self.state.text_input_manager.is_some()
    }

    pub(crate) fn enable_text_input(
        &mut self,
        context: TextInputContext,
    ) -> Result<Option<u32>, String> {
        if !self.state.text_input_focused {
            return Ok(None);
        }
        let Some(input) = self.state.text_input.clone() else {
            return Ok(None);
        };
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
        let serial = self.state.bump_text_input_commit();
        self.flush()?;
        Ok(serial)
    }

    pub(crate) fn update_text_input(
        &mut self,
        context: TextInputContext,
    ) -> Result<Option<u32>, String> {
        if !self.state.text_input_focused {
            return Ok(None);
        }
        let Some(input) = self.state.text_input.as_ref() else {
            return Ok(None);
        };
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
        let serial = self.state.bump_text_input_commit();
        self.flush()?;
        Ok(serial)
    }

    pub(crate) fn disable_text_input(&mut self) -> Result<Option<u32>, String> {
        let Some(input) = self.state.text_input.as_ref() else {
            return Ok(None);
        };
        input.set_surrounding_text(String::new(), 0, 0);
        input.set_content_type(ContentHint::None, ContentPurpose::Terminal);
        input.disable();
        input.commit();
        self.surface.commit();
        let serial = self.state.bump_text_input_commit();
        self.flush()?;
        Ok(serial)
    }

    pub(crate) fn publish_selection(
        &mut self,
        target: SelectionTarget,
        source_id: u64,
        serial: InputSerial,
    ) -> Result<bool, String> {
        const MIMES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
        if serial.seat != self.state.seat_token {
            return Err("selection publish rejected a stale seat serial".into());
        }
        let qh = self.event_queue.handle();
        match target {
            SelectionTarget::Clipboard => {
                let Some(device) = self.state.data_device.as_ref() else {
                    return Ok(false);
                };
                let source = self
                    .state
                    .data_device_manager
                    .create_copy_paste_source(&qh, MIMES);
                source.set_selection(device, serial.value);
                self.state.clipboard_source = Some((source_id, source));
            }
            SelectionTarget::Primary => {
                let (Some(manager), Some(device)) = (
                    self.state.primary_selection_manager.as_ref(),
                    self.state.primary_device.as_ref(),
                ) else {
                    return Ok(false);
                };
                let source = manager.create_selection_source(&qh, MIMES);
                source.set_selection(device, serial.value);
                self.state.primary_source = Some((source_id, source));
            }
        }
        self.flush()?;
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
        let fd = match target {
            SelectionTarget::Clipboard => {
                let Some(offer) = self.state.clipboard_offer.as_ref() else {
                    return Ok(None);
                };
                let Some(mime) = offer.with_mime_types(choose_mime) else {
                    return Ok(None);
                };
                offer.receive(mime).ok().map(OwnedFd::from)
            }
            SelectionTarget::Primary => {
                let Some(offer) = self.state.primary_offer.as_ref() else {
                    return Ok(None);
                };
                let Some(mime) = offer.with_mime_types(choose_mime) else {
                    return Ok(None);
                };
                offer.receive(mime).ok().map(OwnedFd::from)
            }
        };
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
    pub(crate) fn set_pointer_cursor(&mut self, cursor: PointerCursor) {
        self.state.pointer_cursor = cursor;
        let edge = self.state.pointer_resize_edge;
        self.state.set_pointer_cursor(&self.connection, edge);
    }
    pub(crate) fn display_ptr(&self) -> *mut c_void {
        self.connection.backend().display_ptr().cast()
    }
    pub(crate) fn surface_ptr(&self) -> *mut c_void {
        self.surface.id().as_ptr().cast()
    }
}

#[allow(clippy::struct_excessive_bools)]
pub(crate) struct WaylandState {
    outputs: OutputState,
    seats: SeatState,
    shm: Shm,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<ThemedPointer>,
    pointer_seat: Option<wl_seat::WlSeat>,
    cursor_surface: wl_surface::WlSurface,
    libdecor_resize_fallback: bool,
    pointer_icon: Option<CursorIcon>,
    pointer_cursor: PointerCursor,
    pointer_resize_edge: Option<ResizeEdge>,
    resize_press_active: bool,
    data_device_manager: DataDeviceManagerState,
    data_device: Option<DataDevice>,
    clipboard_offer: Option<SelectionOffer>,
    clipboard_source: Option<(u64, CopyPasteSource)>,
    primary_selection_manager: Option<PrimarySelectionManagerState>,
    primary_device: Option<PrimarySelectionDevice>,
    primary_offer: Option<PrimarySelectionOffer>,
    primary_source: Option<(u64, PrimarySelectionSource)>,
    text_input_manager: Option<ZwpTextInputManagerV3>,
    text_input: Option<ZwpTextInputV3>,
    text_input_focused: bool,
    text_input_commits: u32,
    target_surface: wl_surface::WlSurface,
    modifiers: ModifiersState,
    pressed_modifiers: PressedModifiers,
    key_repeat: KeyRepeatState,
    shortcut_digit_rows: HashMap<u32, std::num::NonZeroU8>,
    logical_size: LogicalSize,
    scale: Scale120,
    pending: PendingEvents,
    pub(crate) configured: bool,
    suspended: bool,
    seat_token: SeatToken,
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
            self.text_input_focused = false;
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
            self.pending
                .push_input(PlatformEvent::Clipboard(ClipboardEvent::Unavailable(
                    SelectionTarget::Clipboard,
                )));
            self.pending
                .push_input(PlatformEvent::Clipboard(ClipboardEvent::Unavailable(
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
        if surface == &self.target_surface {
            tracing::debug!("Wayland keyboard focus entered terminal surface");
            self.pending.push_input(PlatformEvent::KeyboardFocus {
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
        if surface == &self.target_surface {
            self.pressed_modifiers = PressedModifiers::default();
            self.key_repeat.cancel();
            self.pending.push_input(PlatformEvent::KeyboardFocus {
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
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = ModifiersState {
            shift: modifiers.shift,
            control: modifiers.ctrl,
            alt: modifiers.alt,
            super_key: modifiers.logo,
            alt_graph: false,
        };
        self.pending
            .push_input(PlatformEvent::ModifiersChanged(self.modifiers));
    }

    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        keymap: Keymap<'_>,
    ) {
        self.shortcut_digit_rows = parse_digit_row_keycodes(&keymap.as_string());
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
        if let Some((serial, event)) = self.key_repeat.take_due(now) {
            self.push_key(serial, event, KeyState::Pressed, true);
        }
    }

    fn ensure_selection_devices(&mut self, qh: &QueueHandle<Self>, seat: &wl_seat::WlSeat) {
        if self.data_device.is_none() {
            self.data_device = Some(self.data_device_manager.get_data_device(qh, seat));
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
        self.pending.push_input(PlatformEvent::Key(KeyInput {
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
            logical_key: logical_key_from_keysym(event.keysym.raw()),
            state,
            repeat,
        }));
    }

    fn bump_text_input_commit(&mut self) -> Option<u32> {
        self.text_input_commits = self.text_input_commits.checked_add(1)?;
        Some(self.text_input_commits)
    }
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
            if event.surface != self.target_surface {
                continue;
            }
            let edge = self
                .libdecor_resize_fallback
                .then(|| content_resize_edge(event.position, self.logical_size))
                .flatten();
            self.pointer_resize_edge = edge;
            let kind = match &event.kind {
                PointerEventKind::Enter { serial } => {
                    // Cursor requests are serial-bound and must be repeated for every enter.
                    self.pointer_icon = None;
                    self.set_pointer_cursor(connection, edge);
                    PointerKind::Enter {
                        serial: self.pointer_serial(*serial),
                    }
                }
                PointerEventKind::Leave { serial } => {
                    self.pointer_resize_edge = None;
                    PointerKind::Leave {
                        serial: self.pointer_serial(*serial),
                    }
                }
                PointerEventKind::Motion { time } => {
                    self.set_pointer_cursor(connection, edge);
                    PointerKind::Motion { time_ms: *time }
                }
                PointerEventKind::Press {
                    time,
                    button,
                    serial,
                } => {
                    self.resize_press_active = false;
                    if *button == 0x110
                        && let (Some(edge), Some(seat)) = (edge, self.pointer_seat.clone())
                    {
                        self.pending.resize = Some((seat, *serial, edge));
                        self.resize_press_active = true;
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
                    if *button == 0x110 && std::mem::take(&mut self.resize_press_active) {
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
            self.pending
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

    fn set_pointer_cursor(&mut self, connection: &Connection, edge: Option<ResizeEdge>) {
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        let icon = match edge {
            Some(ResizeEdge::Top) => CursorIcon::NResize,
            Some(ResizeEdge::Bottom) => CursorIcon::SResize,
            Some(ResizeEdge::Left) => CursorIcon::WResize,
            Some(ResizeEdge::TopLeft) => CursorIcon::NwResize,
            Some(ResizeEdge::BottomLeft) => CursorIcon::SwResize,
            Some(ResizeEdge::Right) => CursorIcon::EResize,
            Some(ResizeEdge::TopRight) => CursorIcon::NeResize,
            Some(ResizeEdge::BottomRight) => CursorIcon::SeResize,
            None => match self.pointer_cursor {
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

    use super::{KeyRepeatState, parse_digit_row_keycodes};

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
}

impl WindowHandler for WaylandState {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.pending.close = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        // A zero/None dimension is a compositor non-preference, not suspension.
        if let Some(width) = configure.new_size.0 {
            self.logical_size.width = width.get();
        }
        if let Some(height) = configure.new_size.1 {
            self.logical_size.height = height.get();
        }
        self.configured = true;
        let suspended = configure.state.contains(XdgWindowState::SUSPENDED);
        if suspended != self.suspended {
            self.suspended = suspended;
            self.pending.suspended = Some(suspended);
        }
        self.pending.configured = Some((
            self.logical_size,
            self.scale,
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
        _: &wl_surface::WlSurface,
        scale: i32,
    ) {
        self.scale = Scale120(u32::try_from(scale.max(1)).unwrap_or(1).saturating_mul(120));
        self.pending.scale = Some(self.scale);
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.pending.frame_ready = true;
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
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let scale = Scale120(scale.max(1));
            if state.scale != scale {
                state.scale = scale;
                state.pending.scale = Some(scale);
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
    fn event(
        state: &mut Self,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let event = match event {
            zwp_text_input_v3::Event::Enter { surface } if surface == state.target_surface => {
                state.text_input_focused = true;
                Some(TextInputEvent::Enter)
            }
            zwp_text_input_v3::Event::Leave { surface } if surface == state.target_surface => {
                state.text_input_focused = false;
                Some(TextInputEvent::Leave)
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } if state.text_input_focused => Some(TextInputEvent::Preedit {
                text: text.unwrap_or_default(),
                cursor: (cursor_begin != -1 || cursor_end != -1)
                    .then_some((cursor_begin, cursor_end)),
            }),
            zwp_text_input_v3::Event::CommitString { text } if state.text_input_focused => {
                Some(TextInputEvent::Commit(text.unwrap_or_default()))
            }
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } if state.text_input_focused => Some(TextInputEvent::DeleteSurrounding {
                before_bytes: before_length,
                after_bytes: after_length,
            }),
            zwp_text_input_v3::Event::Done { serial } if state.text_input_focused => {
                Some(TextInputEvent::Done { serial })
            }
            _ => None,
        };
        if let Some(event) = event {
            state.pending.push_input(PlatformEvent::TextInput(event));
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
        self.pending.push_input(PlatformEvent::Clipboard(event));
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
            self.pending
                .push_input(PlatformEvent::Clipboard(ClipboardEvent::Send {
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
            self.pending
                .push_input(PlatformEvent::Clipboard(ClipboardEvent::SourceCancelled {
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
        self.pending.push_input(PlatformEvent::Clipboard(event));
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
            self.pending
                .push_input(PlatformEvent::Clipboard(ClipboardEvent::Send {
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
            self.pending
                .push_input(PlatformEvent::Clipboard(ClipboardEvent::SourceCancelled {
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
}

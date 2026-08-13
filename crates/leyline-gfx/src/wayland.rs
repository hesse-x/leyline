use std::{
    env,
    ffi::c_void,
    os::fd::{AsFd, BorrowedFd},
    time::Duration,
};

use rustix::{
    event::{PollFd, PollFlags, poll},
    time::Timespec,
};

use smithay_client_toolkit::reexports::csd_frame::WindowState as XdgWindowState;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::WaylandError,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_output, wl_registry, wl_surface},
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{
        wp_viewport::{self, WpViewport},
        wp_viewporter::WpViewporter,
    },
};

use crate::decor::Libdecor;
use crate::{GfxInitError, LogicalSize, PlatformEvent, Scale120, WindowState};

const DEFAULT_SIZE: LogicalSize = LogicalSize {
    width: 800,
    height: 500,
};

#[derive(Default)]
struct PendingEvents {
    close: bool,
    configured: Option<(LogicalSize, Scale120, WindowState)>,
    scale: Option<Scale120>,
    frame_ready: bool,
    suspended: Option<bool>,
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

impl WaylandWindow {
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
        let shell = XdgShell::bind(&globals, &qh)
            .map_err(|error| GfxInitError::Platform(format!("cannot bind xdg-shell: {error}")))?;
        let surface = compositor.create_surface(&qh);
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
                logical_size: DEFAULT_SIZE,
                scale: Scale120::ONE,
                pending: PendingEvents::default(),
                configured: false,
                suspended: false,
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
        if let Some(decor) = self.libdecor.as_mut() {
            decor.dispatch(0)?;
            if let Some(size) = decor.take_configured() {
                self.state.logical_size = size;
                self.state.configured = true;
                self.state.pending.configured =
                    Some((size, self.state.scale, WindowState::default()));
            }
            self.state.pending.close |= decor.take_close();
            if decor.take_commit_requested() {
                self.surface.commit();
            }
        }
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map(|_| ())
            .map_err(|error| format!("Wayland dispatch failed: {error}"))
    }

    pub(crate) fn roundtrip(&mut self) -> Result<(), String> {
        if let Some(decor) = self.libdecor.as_mut() {
            decor.dispatch(50)?;
            if let Some(size) = decor.take_configured() {
                self.state.logical_size = size;
                self.state.configured = true;
                self.state.pending.configured =
                    Some((size, self.state.scale, WindowState::default()));
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
        self.flush()?;
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
        let mut descriptors = vec![PollFd::from_borrowed_fd(
            self.connection.as_fd(),
            PollFlags::IN | PollFlags::ERR | PollFlags::HUP,
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
        if descriptors[0]
            .revents()
            .intersects(PollFlags::ERR | PollFlags::HUP)
        {
            return Err("Wayland compositor disconnected".into());
        }
        if descriptors[0].revents().contains(PollFlags::IN) {
            read_guard
                .read()
                .map_err(|error| format!("Wayland socket read failed: {error}"))?;
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

    pub(crate) fn take_events(&mut self, output: &mut Vec<PlatformEvent>) {
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
    pub(crate) fn set_title(&mut self, title: &str) {
        if let Some(window) = self.window.as_ref() {
            window.set_title(title);
        }
        // Validation in the safe facade already excludes NUL.
        if let Some(decor) = self.libdecor.as_mut() {
            let _ = decor.set_title(title);
        }
    }
    pub(crate) fn display_ptr(&self) -> *mut c_void {
        self.connection.backend().display_ptr().cast()
    }
    pub(crate) fn surface_ptr(&self) -> *mut c_void {
        self.surface.id().as_ptr().cast()
    }
}

pub(crate) struct WaylandState {
    outputs: OutputState,
    logical_size: LogicalSize,
    scale: Scale120,
    pending: PendingEvents,
    pub(crate) configured: bool,
    suspended: bool,
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
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

delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_xdg_shell!(WaylandState);
delegate_xdg_window!(WaylandState);

#![allow(unsafe_code)]

use std::env;

use smithay_client_toolkit::activation::{ActivationHandler, ActivationState, RequestData};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    DecorationMode, Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_activation, delegate_compositor, delegate_output, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window,
};
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

use crate::report::{ProbeError, ProbeResult, Reporter};

const DEFAULT_SIZE: (u32, u32) = (320, 200);

pub struct SurfaceHarness {
    window: Window,
    pub surface: wl_surface::WlSurface,
    event_queue: EventQueue<WindowState>,
    state: WindowState,
    _pool: SlotPool,
    _buffer: Buffer,
    pub connection: Connection,
}

impl SurfaceHarness {
    pub fn create(interactive: bool) -> ProbeResult<Self> {
        if env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err(ProbeError::missing(
                "wayland.connect",
                "WAYLAND_DISPLAY is unset",
                "run inside the Ubuntu GNOME Wayland session",
            ));
        }
        let connection = Connection::connect_to_env().map_err(|error| {
            ProbeError::missing(
                "wayland.connect",
                error.to_string(),
                "verify WAYLAND_DISPLAY and access to the compositor socket",
            )
        })?;
        let (globals, mut event_queue) = registry_queue_init(&connection).map_err(|error| {
            ProbeError::protocol(
                "wayland.registry",
                error.to_string(),
                "inspect the compositor connection with WAYLAND_DEBUG=client",
            )
        })?;
        ensure_globals(&globals)?;
        let qh = event_queue.handle();
        let compositor = CompositorState::bind(&globals, &qh).map_err(|error| {
            ProbeError::protocol(
                "wayland.compositor",
                error.to_string(),
                "provide wl_compositor",
            )
        })?;
        let shell = XdgShell::bind(&globals, &qh).map_err(|error| {
            ProbeError::protocol(
                "wayland.xdg-shell",
                error.to_string(),
                "provide stable xdg-shell",
            )
        })?;
        let shm = Shm::bind(&globals, &qh).map_err(|error| {
            ProbeError::protocol(
                "wayland.shm",
                error.to_string(),
                "provide wl_shm for the visible interaction probe",
            )
        })?;
        let activation = ActivationState::bind(&globals, &qh).ok();
        let surface = compositor.create_surface(&qh);
        let window = shell.create_window(surface.clone(), WindowDecorations::RequestServer, &qh);
        window.set_title("FastTerm stage 0 probe");
        window.set_app_id("io.fastterm.Stage0Probe");
        window.set_min_size(Some(DEFAULT_SIZE));
        window.commit();
        let mut pool = SlotPool::new(DEFAULT_SIZE.0 as usize * DEFAULT_SIZE.1 as usize * 4, &shm)
            .map_err(|error| ProbeError::internal("wayland.shm", error.to_string()))?;
        let mut state = WindowState::new(&globals, &qh, shm, activation);
        for _ in 0..4 {
            event_queue.roundtrip(&mut state).map_err(|error| {
                ProbeError::protocol(
                    "wayland.configure",
                    error.to_string(),
                    "inspect the compositor with WAYLAND_DEBUG=client",
                )
            })?;
            if state.configured {
                break;
            }
        }
        if !state.configured {
            return Err(ProbeError::protocol(
                "wayland.configure",
                "no initial xdg_surface configure after four roundtrips",
                "verify the compositor accepts xdg_toplevel creation",
            ));
        }
        if !interactive {
            exercise_window_states(&window, &mut event_queue, &mut state)?;
        }
        let buffer = attach_visible_buffer(&window, &mut pool)?;
        if let Some(activation) = state.activation.as_ref() {
            activation.request_token(
                &qh,
                RequestData {
                    app_id: Some("io.fastterm.Stage0Probe".into()),
                    seat_and_serial: None,
                    surface: Some(window.wl_surface().clone()),
                },
            );
            window.commit();
        }
        Ok(Self {
            window,
            surface,
            event_queue,
            state,
            _pool: pool,
            _buffer: buffer,
            connection,
        })
    }

    fn capabilities(&self) -> String {
        format!(
            "size={}x{}; decoration={:?}; configures={}; scale={}; output_enters={}; maximized_seen={}; fullscreen_seen={}; close_callback_wired=true",
            self.state.size.0,
            self.state.size.1,
            self.state.decoration,
            self.state.configures,
            self.state.scale,
            self.state.output_enters,
            self.state.maximized_seen,
            self.state.fullscreen_seen,
        )
    }

    pub fn display_ptr(&self) -> *mut std::ffi::c_void {
        self.connection.backend().display_ptr().cast()
    }

    pub fn surface_ptr(&self) -> *mut std::ffi::c_void {
        self.surface.id().as_ptr().cast()
    }
}

fn attach_visible_buffer(window: &Window, pool: &mut SlotPool) -> ProbeResult<Buffer> {
    attach_surface_buffer(window.wl_surface(), pool)
}

fn attach_surface_buffer(
    surface: &wl_surface::WlSurface,
    pool: &mut SlotPool,
) -> ProbeResult<Buffer> {
    attach_sized_surface_buffer(surface, pool, DEFAULT_SIZE.0, DEFAULT_SIZE.1)
}

fn attach_sized_surface_buffer(
    surface: &wl_surface::WlSurface,
    pool: &mut SlotPool,
    width: u32,
    height: u32,
) -> ProbeResult<Buffer> {
    let width = i32::try_from(width)
        .map_err(|_| ProbeError::internal("wayland.buffer", "width exceeds i32"))?;
    let height = i32::try_from(height)
        .map_err(|_| ProbeError::internal("wayland.buffer", "height exceeds i32"))?;
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| ProbeError::internal("wayland.buffer", "stride overflow"))?;
    let (buffer, canvas) = pool
        .create_buffer(width, height, stride, wl_shm::Format::Xrgb8888)
        .map_err(|error| ProbeError::internal("wayland.buffer", error.to_string()))?;
    for pixel in canvas.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0x32, 0x44, 0x28, 0xff]);
    }
    buffer
        .attach_to(surface)
        .map_err(|error| ProbeError::internal("wayland.buffer", error.to_string()))?;
    surface.damage_buffer(0, 0, width, height);
    surface.commit();
    Ok(buffer)
}

impl Drop for SurfaceHarness {
    fn drop(&mut self) {
        // Keep the queue and state alive until after the xdg objects have been explicitly destroyed.
        self.window.wl_surface().attach(None, 0, 0);
        self.window.commit();
        let _ = self.event_queue.flush();
    }
}

pub fn run(reporter: &mut Reporter, interactive_seconds: Option<u64>) -> ProbeResult<()> {
    let harness = SurfaceHarness::create(false)?;
    reporter.pass("wayland", "window-lifecycle", harness.capabilities());
    reporter.note(
        "wayland",
        "interaction",
        "maximize and fullscreen transitions were exercised; close, interactive resize, scale, and output movement callbacks are wired",
    );
    drop(harness);
    libdecor::validate(reporter, interactive_seconds)?;
    reporter.pass(
        "wayland",
        "destruction-order",
        "libdecor frame -> libdecor context -> wl_surface -> xdg window -> event queue -> display",
    );
    Ok(())
}

fn ensure_globals(globals: &GlobalList) -> ProbeResult<()> {
    let advertised = globals.contents().clone_list();
    for required in ["wl_compositor", "xdg_wm_base"] {
        if !advertised.iter().any(|global| global.interface == required) {
            return Err(ProbeError::protocol(
                "wayland.globals",
                format!("required global {required} is missing"),
                "use a compositor implementing stable XDG shell",
            ));
        }
    }
    Ok(())
}

fn exercise_window_states(
    window: &Window,
    event_queue: &mut EventQueue<WindowState>,
    state: &mut WindowState,
) -> ProbeResult<()> {
    window.set_maximized();
    window.commit();
    for _ in 0..4 {
        event_queue.roundtrip(state).map_err(|error| {
            ProbeError::protocol(
                "wayland.maximize",
                error.to_string(),
                "inspect xdg_toplevel state handling",
            )
        })?;
        if state.maximized_seen {
            break;
        }
    }
    window.unset_maximized();
    window.set_fullscreen(None);
    window.commit();
    for _ in 0..4 {
        event_queue.roundtrip(state).map_err(|error| {
            ProbeError::protocol(
                "wayland.fullscreen",
                error.to_string(),
                "inspect xdg_toplevel state handling",
            )
        })?;
        if state.fullscreen_seen {
            break;
        }
    }
    window.unset_fullscreen();
    window.commit();
    if !state.maximized_seen || !state.fullscreen_seen {
        return Err(ProbeError::protocol(
            "wayland.window-states",
            format!(
                "maximized_seen={} fullscreen_seen={}",
                state.maximized_seen, state.fullscreen_seen
            ),
            "verify GNOME permits xdg_toplevel maximize/fullscreen transitions",
        ));
    }
    Ok(())
}

#[allow(clippy::struct_excessive_bools)]
struct WindowState {
    outputs: OutputState,
    shm: Shm,
    activation: Option<ActivationState>,
    configured: bool,
    close_requested: bool,
    configures: u32,
    size: (u32, u32),
    decoration: DecorationMode,
    scale: i32,
    output_enters: u32,
    maximized_seen: bool,
    fullscreen_seen: bool,
    resized_seen: bool,
    output_leaves: u32,
}

impl WindowState {
    fn new(
        globals: &GlobalList,
        qh: &QueueHandle<Self>,
        shm: Shm,
        activation: Option<ActivationState>,
    ) -> Self {
        Self {
            outputs: OutputState::new(globals, qh),
            shm,
            activation,
            configured: false,
            close_requested: false,
            configures: 0,
            size: DEFAULT_SIZE,
            decoration: DecorationMode::Client,
            scale: 1,
            output_enters: 0,
            maximized_seen: false,
            fullscreen_seen: false,
            resized_seen: false,
            output_leaves: 0,
        }
    }
}

impl OutputHandler for WindowState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for WindowState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ActivationHandler for WindowState {
    type RequestData = RequestData;

    fn new_token(&mut self, token: String, data: &RequestData) {
        if let (Some(activation), Some(surface)) = (self.activation.as_ref(), data.surface.as_ref())
        {
            activation.activate::<WindowState>(surface, token);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WindowState {
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

impl WindowHandler for WindowState {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.close_requested = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        self.configured = true;
        self.configures = self.configures.saturating_add(1);
        self.size = (
            configure
                .new_size
                .0
                .map_or(DEFAULT_SIZE.0, std::num::NonZeroU32::get),
            configure
                .new_size
                .1
                .map_or(DEFAULT_SIZE.1, std::num::NonZeroU32::get),
        );
        self.resized_seen |= self.configures > 1 && self.size != DEFAULT_SIZE;
        self.decoration = configure.decoration_mode;
        self.maximized_seen |= configure.is_maximized();
        self.fullscreen_seen |= configure.is_fullscreen();
    }
}

impl CompositorHandler for WindowState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        scale: i32,
    ) {
        self.scale = scale.max(1);
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
        self.output_enters = self.output_enters.saturating_add(1);
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
        self.output_leaves = self.output_leaves.saturating_add(1);
    }
}

delegate_compositor!(WindowState);
delegate_activation!(WindowState);
delegate_output!(WindowState);
delegate_shm!(WindowState);
delegate_xdg_shell!(WindowState);
delegate_xdg_window!(WindowState);

#[cfg(has_decor)]
mod libdecor {
    use smithay_client_toolkit::compositor::CompositorState;
    use smithay_client_toolkit::shm::Shm;
    use smithay_client_toolkit::shm::slot::SlotPool;
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    use std::time::{Duration, Instant};
    use wayland_client::globals::registry_queue_init;
    use wayland_client::{Connection, Proxy};

    use super::{ProbeError, ProbeResult, Reporter};

    enum Context {}
    enum Frame {}
    enum Configuration {}
    enum DecorState {}

    #[repr(C)]
    struct ContextInterface {
        error: Option<unsafe extern "C" fn(*mut Context, c_int, *const c_char)>,
        reserved: [Option<unsafe extern "C" fn()>; 10],
    }

    #[repr(C)]
    struct FrameInterface {
        configure: Option<unsafe extern "C" fn(*mut Frame, *mut Configuration, *mut c_void)>,
        close: Option<unsafe extern "C" fn(*mut Frame, *mut c_void)>,
        commit: Option<unsafe extern "C" fn(*mut Frame, *mut c_void)>,
        dismiss_popup: Option<unsafe extern "C" fn(*mut Frame, *const c_char, *mut c_void)>,
        reserved: [Option<unsafe extern "C" fn()>; 10],
    }

    #[derive(Default)]
    #[allow(clippy::struct_excessive_bools)]
    struct CallbackState {
        configured: bool,
        closed: bool,
        commits: u32,
        configures: u32,
        initial_size: Option<(i32, i32)>,
        resized: bool,
        content_resized: bool,
        width: i32,
        height: i32,
    }

    #[link(name = "decor-0")]
    unsafe extern "C" {
        fn libdecor_new(display: *mut c_void, interface: *const ContextInterface) -> *mut Context;
        fn libdecor_unref(context: *mut Context);
        fn libdecor_dispatch(context: *mut Context, timeout: c_int) -> c_int;
        fn libdecor_decorate(
            context: *mut Context,
            surface: *mut c_void,
            interface: *const FrameInterface,
            user_data: *mut c_void,
        ) -> *mut Frame;
        fn libdecor_frame_unref(frame: *mut Frame);
        fn libdecor_frame_set_title(frame: *mut Frame, title: *const c_char);
        fn libdecor_frame_set_app_id(frame: *mut Frame, app_id: *const c_char);
        fn libdecor_frame_map(frame: *mut Frame);
        fn libdecor_configuration_get_content_size(
            configuration: *mut Configuration,
            frame: *mut Frame,
            width: *mut c_int,
            height: *mut c_int,
        ) -> bool;
        fn libdecor_state_new(width: c_int, height: c_int) -> *mut DecorState;
        fn libdecor_state_free(state: *mut DecorState);
        fn libdecor_frame_commit(
            frame: *mut Frame,
            state: *mut DecorState,
            configuration: *mut Configuration,
        );
    }

    unsafe extern "C" fn context_error(_: *mut Context, _: c_int, message: *const c_char) {
        if !message.is_null() {
            eprintln!(
                "libdecor: {}",
                unsafe { CStr::from_ptr(message) }.to_string_lossy()
            );
        }
    }

    unsafe extern "C" fn configure(
        frame: *mut Frame,
        configuration: *mut Configuration,
        user_data: *mut c_void,
    ) {
        let Some(state) = (unsafe { user_data.cast::<CallbackState>().as_mut() }) else {
            return;
        };
        let mut width = 320;
        let mut height = 200;
        unsafe {
            libdecor_configuration_get_content_size(
                configuration,
                frame,
                &raw mut width,
                &raw mut height,
            );
        }
        width = width.max(1);
        height = height.max(1);
        let content = unsafe { libdecor_state_new(width, height) };
        if content.is_null() {
            return;
        }
        unsafe {
            libdecor_frame_commit(frame, content, configuration);
            libdecor_state_free(content);
        }
        state.configured = true;
        state.configures = state.configures.saturating_add(1);
        let size = (width, height);
        if let Some(initial_size) = state.initial_size {
            state.resized |= size != initial_size;
        } else {
            state.initial_size = Some(size);
        }
        state.width = width;
        state.height = height;
    }

    unsafe extern "C" fn close(_: *mut Frame, user_data: *mut c_void) {
        if let Some(state) = unsafe { user_data.cast::<CallbackState>().as_mut() } {
            state.closed = true;
        }
    }

    unsafe extern "C" fn commit(_: *mut Frame, user_data: *mut c_void) {
        if let Some(state) = unsafe { user_data.cast::<CallbackState>().as_mut() } {
            state.commits = state.commits.saturating_add(1);
        }
    }

    unsafe extern "C" fn dismiss_popup(_: *mut Frame, _: *const c_char, _: *mut c_void) {}

    static CONTEXT_INTERFACE: ContextInterface = ContextInterface {
        error: Some(context_error),
        reserved: [None; 10],
    };
    static FRAME_INTERFACE: FrameInterface = FrameInterface {
        configure: Some(configure),
        close: Some(close),
        commit: Some(commit),
        dismiss_popup: Some(dismiss_popup),
        reserved: [None; 10],
    };

    #[allow(clippy::too_many_lines)]
    pub fn validate(reporter: &mut Reporter, interactive_seconds: Option<u64>) -> ProbeResult<()> {
        let connection = Connection::connect_to_env().map_err(|error| {
            ProbeError::missing(
                "libdecor.connect",
                error.to_string(),
                "run in the target Wayland session",
            )
        })?;
        let (globals, event_queue) = registry_queue_init::<super::WindowState>(&connection)
            .map_err(|error| {
                ProbeError::protocol(
                    "libdecor.registry",
                    error.to_string(),
                    "inspect Wayland globals",
                )
            })?;
        let compositor =
            CompositorState::bind(&globals, &event_queue.handle()).map_err(|error| {
                ProbeError::protocol(
                    "libdecor.compositor",
                    error.to_string(),
                    "provide wl_compositor",
                )
            })?;
        let shm = Shm::bind(&globals, &event_queue.handle()).map_err(|error| {
            ProbeError::protocol(
                "libdecor.shm",
                error.to_string(),
                "provide wl_shm for the libdecor content surface",
            )
        })?;
        let surface = compositor.create_surface(&event_queue.handle());
        let mut pool = SlotPool::new(
            super::DEFAULT_SIZE.0 as usize * super::DEFAULT_SIZE.1 as usize * 4,
            &shm,
        )
        .map_err(|error| ProbeError::internal("libdecor.shm", error.to_string()))?;
        let mut callback_state = Box::<CallbackState>::default();
        // SAFETY: the context, frame, callback state, connection, and surface stay alive for the
        // complete dispatch loop. Destruction is frame before context before the Wayland objects.
        unsafe {
            let context = libdecor_new(
                connection.backend().display_ptr().cast(),
                &raw const CONTEXT_INTERFACE,
            );
            if context.is_null() {
                return Err(ProbeError::internal(
                    "libdecor.context",
                    "libdecor_new returned null",
                ));
            }
            let frame = libdecor_decorate(
                context,
                surface.id().as_ptr().cast(),
                &raw const FRAME_INTERFACE,
                (&raw mut *callback_state).cast(),
            );
            if frame.is_null() {
                libdecor_unref(context);
                return Err(ProbeError::internal(
                    "libdecor.frame",
                    "libdecor_decorate returned null",
                ));
            }
            let title = CString::new("FastTerm libdecor probe")
                .map_err(|_| ProbeError::internal("libdecor.title", "invalid title"))?;
            let app_id = CString::new("io.fastterm.LibdecorProbe")
                .map_err(|_| ProbeError::internal("libdecor.app-id", "invalid app id"))?;
            libdecor_frame_set_title(frame, title.as_ptr());
            libdecor_frame_set_app_id(frame, app_id.as_ptr());
            libdecor_frame_map(frame);
            surface.commit();
            for _ in 0..20 {
                if callback_state.configured {
                    break;
                }
                if libdecor_dispatch(context, 50) < 0 {
                    break;
                }
            }
            let mut rendered_size = (callback_state.width, callback_state.height);
            let mut buffer = super::attach_sized_surface_buffer(
                &surface,
                &mut pool,
                u32::try_from(rendered_size.0).map_err(|_| {
                    ProbeError::internal("libdecor.buffer", "negative configured width")
                })?,
                u32::try_from(rendered_size.1).map_err(|_| {
                    ProbeError::internal("libdecor.buffer", "negative configured height")
                })?,
            )?;
            if let Some(seconds) = interactive_seconds {
                if seconds == 0 || seconds > 300 {
                    libdecor_frame_unref(frame);
                    libdecor_unref(context);
                    return Err(ProbeError::internal(
                        "libdecor.interaction",
                        "interactive timeout must be between 1 and 300 seconds",
                    ));
                }
                eprintln!(
                    "Resize the FastTerm libdecor probe window, then close it (timeout: {seconds}s)."
                );
                let deadline = Instant::now() + Duration::from_secs(seconds);
                while Instant::now() < deadline && !callback_state.closed {
                    if libdecor_dispatch(context, 50) < 0 {
                        break;
                    }
                    let configured_size = (callback_state.width, callback_state.height);
                    if configured_size != rendered_size {
                        let new_buffer = super::attach_sized_surface_buffer(
                            &surface,
                            &mut pool,
                            u32::try_from(configured_size.0).map_err(|_| {
                                ProbeError::internal("libdecor.buffer", "negative configured width")
                            })?,
                            u32::try_from(configured_size.1).map_err(|_| {
                                ProbeError::internal(
                                    "libdecor.buffer",
                                    "negative configured height",
                                )
                            })?,
                        )?;
                        buffer = new_buffer;
                        rendered_size = configured_size;
                        callback_state.content_resized = true;
                    }
                }
            }
            drop(buffer);
            libdecor_frame_unref(frame);
            libdecor_unref(context);
        }
        if !callback_state.configured {
            return Err(ProbeError::protocol(
                "libdecor.configure",
                "no configure callback within one second",
                "verify the GNOME libdecor plugin and compositor",
            ));
        }
        if interactive_seconds.is_some()
            && (!callback_state.resized
                || !callback_state.content_resized
                || !callback_state.closed)
        {
            return Err(ProbeError::protocol(
                "libdecor.interaction",
                format!(
                    "configured_resized={}; content_resized={}; close_requested={}; configures={}; final_size={}x{}",
                    callback_state.resized,
                    callback_state.content_resized,
                    callback_state.closed,
                    callback_state.configures,
                    callback_state.width,
                    callback_state.height
                ),
                "resize the libdecor window and close it before the timeout",
            ));
        }
        reporter.pass(
            "wayland",
            "libdecor",
            format!(
                "configured={}x{}; configures={}; configured_resized={}; content_resized={}; commit_callbacks={}; close_requested={}",
                callback_state.width,
                callback_state.height,
                callback_state.configures,
                callback_state.resized,
                callback_state.content_resized,
                callback_state.commits,
                callback_state.closed
            ),
        );
        Ok(())
    }
}

#[cfg(not(has_decor))]
mod libdecor {
    use super::{ProbeError, ProbeResult, Reporter};

    pub fn validate(_: &mut Reporter, _: Option<u64>) -> ProbeResult<()> {
        Err(ProbeError::missing(
            "libdecor.build",
            "libdecor-0 development metadata was absent at build time",
            "install libdecor-0-dev and rebuild",
        ))
    }
}

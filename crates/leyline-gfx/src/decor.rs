#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};

use wayland_client::{
    Connection, Proxy,
    protocol::{wl_seat, wl_surface},
};

use crate::LogicalSize;

enum Context {}
enum Frame {}
enum Configuration {}
enum DecorState {}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) enum ResizeEdge {
    Top = 1,
    Bottom = 2,
    Left = 3,
    TopLeft = 4,
    BottomLeft = 5,
    Right = 6,
    TopRight = 7,
    BottomRight = 8,
}

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
struct CallbackState {
    size: Option<LogicalSize>,
    pending_size: Option<LogicalSize>,
    close: bool,
    commit_requested: bool,
    fatal: Option<String>,
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
    fn libdecor_frame_set_min_content_size(frame: *mut Frame, width: c_int, height: c_int);
    fn libdecor_frame_resize(frame: *mut Frame, seat: *mut c_void, serial: u32, edge: ResizeEdge);
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
        tracing::error!(category = "platform", message = %unsafe { CStr::from_ptr(message) }.to_string_lossy(), "libdecor error");
    }
}

unsafe extern "C" fn configure(
    frame: *mut Frame,
    configuration: *mut Configuration,
    user_data: *mut c_void,
) {
    let Some(callbacks) = (unsafe { user_data.cast::<CallbackState>().as_mut() }) else {
        return;
    };
    let (mut width, mut height) = callbacks.size.map_or((800, 500), |size| {
        (
            i32::try_from(size.width).unwrap_or(i32::MAX),
            i32::try_from(size.height).unwrap_or(i32::MAX),
        )
    });
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
    let state = unsafe { libdecor_state_new(width, height) };
    if state.is_null() {
        callbacks.fatal = Some("libdecor_state_new returned null".into());
        return;
    }
    unsafe {
        libdecor_frame_commit(frame, state, configuration);
        libdecor_state_free(state);
    }
    let size = LogicalSize {
        width: u32::try_from(width).expect("positive libdecor width"),
        height: u32::try_from(height).expect("positive libdecor height"),
    };
    tracing::debug!(
        category = "platform",
        width = size.width,
        height = size.height,
        "libdecor configured content"
    );
    callbacks.size = Some(size);
    callbacks.pending_size = Some(size);
}

unsafe extern "C" fn close(_: *mut Frame, user_data: *mut c_void) {
    if let Some(callbacks) = unsafe { user_data.cast::<CallbackState>().as_mut() } {
        callbacks.close = true;
    }
}
unsafe extern "C" fn commit(_: *mut Frame, user_data: *mut c_void) {
    if let Some(callbacks) = unsafe { user_data.cast::<CallbackState>().as_mut() } {
        callbacks.commit_requested = true;
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

pub(crate) struct Libdecor {
    context: *mut Context,
    frame: *mut Frame,
    callbacks: Box<CallbackState>,
}

impl Libdecor {
    pub(crate) fn new(
        connection: &Connection,
        surface: &wl_surface::WlSurface,
        title: &str,
    ) -> Result<Self, String> {
        let mut callbacks = Box::<CallbackState>::default();
        // SAFETY: callbacks has a stable Box address and outlives frame/context; native objects are UI-thread-only.
        unsafe {
            let context = libdecor_new(
                connection.backend().display_ptr().cast(),
                &raw const CONTEXT_INTERFACE,
            );
            if context.is_null() {
                return Err("libdecor_new returned null".into());
            }
            let frame = libdecor_decorate(
                context,
                surface.id().as_ptr().cast(),
                &raw const FRAME_INTERFACE,
                (&raw mut *callbacks).cast(),
            );
            if frame.is_null() {
                libdecor_unref(context);
                return Err("libdecor_decorate returned null".into());
            }
            let mut result = Self {
                context,
                frame,
                callbacks,
            };
            result.set_title(title)?;
            let app_id = CString::new("io.leyline.Leyline").expect("static app id");
            libdecor_frame_set_app_id(frame, app_id.as_ptr());
            libdecor_frame_set_min_content_size(frame, 160, 90);
            libdecor_frame_map(frame);
            surface.commit();
            Ok(result)
        }
    }

    pub(crate) fn dispatch(&mut self, timeout_ms: i32) -> Result<(), String> {
        if unsafe { libdecor_dispatch(self.context, timeout_ms) } < 0 {
            return Err("libdecor dispatch failed".into());
        }
        if let Some(error) = self.callbacks.fatal.take() {
            return Err(error);
        }
        Ok(())
    }
    pub(crate) fn take_configured(&mut self) -> Option<LogicalSize> {
        self.callbacks.pending_size.take()
    }
    pub(crate) fn take_close(&mut self) -> bool {
        std::mem::take(&mut self.callbacks.close)
    }
    pub(crate) fn take_commit_requested(&mut self) -> bool {
        std::mem::take(&mut self.callbacks.commit_requested)
    }
    pub(crate) fn set_title(&mut self, title: &str) -> Result<(), String> {
        let title = CString::new(title).map_err(|_| "window title contains NUL")?;
        unsafe { libdecor_frame_set_title(self.frame, title.as_ptr()) };
        Ok(())
    }

    pub(crate) fn resize(&mut self, seat: &wl_seat::WlSeat, serial: u32, edge: ResizeEdge) {
        // SAFETY: the seat belongs to the same display and the serial comes from its pointer press.
        unsafe { libdecor_frame_resize(self.frame, seat.id().as_ptr().cast(), serial, edge) };
    }
}

impl Drop for Libdecor {
    fn drop(&mut self) {
        // SAFETY: frame belongs to context and both are destroyed on their owning UI thread.
        unsafe {
            libdecor_frame_unref(self.frame);
            libdecor_unref(self.context);
        }
    }
}

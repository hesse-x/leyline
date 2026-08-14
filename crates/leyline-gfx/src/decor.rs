#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};

use wayland_client::{
    Connection, Proxy,
    protocol::{wl_seat, wl_surface},
};

use crate::{LogicalSize, WindowState};

const DEFAULT_CONTENT_SIZE: LogicalSize = LogicalSize {
    width: 800,
    height: 500,
};
const WINDOW_STATE_ACTIVE: u32 = 1 << 0;
const WINDOW_STATE_MAXIMIZED: u32 = 1 << 1;
const WINDOW_STATE_FULLSCREEN: u32 = 1 << 2;

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
    window_state: Option<u32>,
    pending_configure: Option<(LogicalSize, WindowState)>,
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
    fn libdecor_configuration_get_window_state(
        configuration: *mut Configuration,
        window_state: *mut u32,
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
    let (mut width, mut height) = (0, 0);
    let has_size = unsafe {
        libdecor_configuration_get_content_size(
            configuration,
            frame,
            &raw mut width,
            &raw mut height,
        )
    };
    let suggested_size = has_size.then(|| LogicalSize {
        width: u32::try_from(width.max(1)).expect("positive libdecor width"),
        height: u32::try_from(height.max(1)).expect("positive libdecor height"),
    });
    let mut configured_state = 0;
    let configured_state = unsafe {
        libdecor_configuration_get_window_state(configuration, &raw mut configured_state)
    }
    .then_some(configured_state);
    let (size, raw_state, activation_only) = resolve_configuration(
        callbacks.size,
        callbacks.window_state,
        suggested_size,
        configured_state,
    );
    width = i32::try_from(size.width).unwrap_or(i32::MAX);
    height = i32::try_from(size.height).unwrap_or(i32::MAX);
    let state = unsafe { libdecor_state_new(width, height) };
    if state.is_null() {
        callbacks.fatal = Some("libdecor_state_new returned null".into());
        return;
    }
    unsafe {
        libdecor_frame_commit(frame, state, configuration);
        libdecor_state_free(state);
    }
    tracing::debug!(
        category = "platform",
        width = size.width,
        height = size.height,
        suggested_width = suggested_size.map(|value| value.width),
        suggested_height = suggested_size.map(|value| value.height),
        window_state = raw_state,
        activation_only,
        "libdecor configured content"
    );
    callbacks.size = Some(size);
    callbacks.window_state = Some(raw_state);
    callbacks.pending_configure = Some((size, semantic_window_state(raw_state)));
}

fn resolve_configuration(
    previous_size: Option<LogicalSize>,
    previous_state: Option<u32>,
    suggested_size: Option<LogicalSize>,
    configured_state: Option<u32>,
) -> (LogicalSize, u32, bool) {
    let next_state = configured_state.or(previous_state).unwrap_or(0);
    let activation_only = previous_state
        .zip(configured_state)
        .is_some_and(|(old, new)| old != new && old ^ new == WINDOW_STATE_ACTIVE);
    // Activation changes do not alter geometry. Mutter/libdecor can nevertheless pair a
    // deactivation configure with the original 800x500 height on a portrait maximized output.
    let size = if activation_only {
        previous_size.or(suggested_size)
    } else {
        suggested_size.or(previous_size)
    }
    .unwrap_or(DEFAULT_CONTENT_SIZE);
    (size, next_state, activation_only)
}

const fn semantic_window_state(state: u32) -> WindowState {
    WindowState {
        maximized: state & WINDOW_STATE_MAXIMIZED != 0,
        fullscreen: state & WINDOW_STATE_FULLSCREEN != 0,
        activated: state & WINDOW_STATE_ACTIVE != 0,
    }
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
    pub(crate) fn take_configured(&mut self) -> Option<(LogicalSize, WindowState)> {
        self.callbacks.pending_configure.take()
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

#[cfg(test)]
mod tests {
    use super::*;

    const MAXIMIZED_ACTIVE: u32 = WINDOW_STATE_MAXIMIZED | WINDOW_STATE_ACTIVE;

    #[test]
    fn activation_only_configure_preserves_maximized_geometry() {
        let current = LogicalSize {
            width: 1440,
            height: 2523,
        };
        let stale_size = LogicalSize {
            width: 1440,
            height: 500,
        };
        let (size, state, activation_only) = resolve_configuration(
            Some(current),
            Some(MAXIMIZED_ACTIVE),
            Some(stale_size),
            Some(WINDOW_STATE_MAXIMIZED),
        );

        assert_eq!(size, current);
        assert_eq!(state, WINDOW_STATE_MAXIMIZED);
        assert!(activation_only);
    }

    #[test]
    fn state_or_real_size_change_accepts_suggested_geometry() {
        let current = LogicalSize {
            width: 800,
            height: 500,
        };
        let maximized = LogicalSize {
            width: 1440,
            height: 2523,
        };
        let (size, state, activation_only) = resolve_configuration(
            Some(current),
            Some(WINDOW_STATE_ACTIVE),
            Some(maximized),
            Some(MAXIMIZED_ACTIVE),
        );

        assert_eq!(size, maximized);
        assert_eq!(state, MAXIMIZED_ACTIVE);
        assert!(!activation_only);
    }
}

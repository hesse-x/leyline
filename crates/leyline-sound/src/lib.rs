use std::ffi::{c_char, c_int, c_void};

use libloading::Library;

type ContextCreate = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type ContextDestroy = unsafe extern "C" fn(*mut c_void) -> c_int;
type ContextChangeProps = unsafe extern "C" fn(*mut c_void, ...) -> c_int;
type ContextPlay = unsafe extern "C" fn(*mut c_void, u32, ...) -> c_int;

pub struct Canberra {
    context: *mut c_void,
    destroy: ContextDestroy,
    play: ContextPlay,
    _library: Library,
}

// The context is exclusively owned and used by the sound worker thread.
unsafe impl Send for Canberra {}

impl Canberra {
    /// Dynamically loads libcanberra and creates one process-local context.
    ///
    /// # Errors
    /// Returns a typed error when the library, required symbols, or context are unavailable.
    pub fn load() -> Result<Self, SoundError> {
        // SAFETY: the soname is fixed and all symbols are copied while the library remains owned.
        let library =
            unsafe { Library::new("libcanberra.so.0") }.map_err(|_| SoundError::Unavailable)?;
        // SAFETY: these names and signatures are from libcanberra's stable public ABI.
        let create = unsafe { *library.get::<ContextCreate>(b"ca_context_create\0")? };
        // SAFETY: see the stable ABI note above.
        let destroy = unsafe { *library.get::<ContextDestroy>(b"ca_context_destroy\0")? };
        // SAFETY: see the stable ABI note above.
        let change_props =
            unsafe { *library.get::<ContextChangeProps>(b"ca_context_change_props\0")? };
        // SAFETY: see the stable ABI note above.
        let play = unsafe { *library.get::<ContextPlay>(b"ca_context_play\0")? };
        let mut context = std::ptr::null_mut();
        // SAFETY: `context` points to writable storage and is checked before later use.
        check(unsafe { create(&raw mut context) })?;
        if context.is_null() {
            return Err(SoundError::Unavailable);
        }
        // SAFETY: the variadic property list consists of static NUL-terminated C strings and
        // ends with a null sentinel, as required by ca_context_change_props.
        let result = unsafe {
            change_props(
                context,
                c"application.id".as_ptr(),
                c"io.leyline.Leyline".as_ptr(),
                std::ptr::null::<c_char>(),
            )
        };
        if let Err(error) = check(result) {
            // SAFETY: context was created successfully and has not yet been destroyed.
            let _ = unsafe { destroy(context) };
            return Err(error);
        }
        Ok(Self {
            context,
            destroy,
            play,
            _library: library,
        })
    }

    /// Plays the desktop sound-theme terminal bell event.
    ///
    /// # Errors
    /// Returns the libcanberra status code when playback cannot be scheduled.
    pub fn play_terminal_bell(&mut self) -> Result<(), SoundError> {
        // SAFETY: the context is live, arguments are static C strings, and the list is terminated.
        check(unsafe {
            (self.play)(
                self.context,
                0,
                c"event.id".as_ptr(),
                c"bell-terminal".as_ptr(),
                std::ptr::null::<c_char>(),
            )
        })
    }
}

impl Drop for Canberra {
    fn drop(&mut self) {
        if !self.context.is_null() {
            // SAFETY: this is the unique live context and Drop runs at most once.
            let _ = unsafe { (self.destroy)(self.context) };
            self.context = std::ptr::null_mut();
        }
    }
}

fn check(status: c_int) -> Result<(), SoundError> {
    if status >= 0 {
        Ok(())
    } else {
        Err(SoundError::Backend(status))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SoundError {
    #[error("libcanberra is unavailable")]
    Unavailable,
    #[error("a required libcanberra symbol is unavailable")]
    Symbol(#[from] libloading::Error),
    #[error("libcanberra returned status {0}")]
    Backend(i32),
}

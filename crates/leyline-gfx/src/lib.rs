//! Safe application facade for Leyline's Wayland/Vulkan boundary.

mod atlas;
mod decor;
mod input;
mod model;
mod runtime;
mod select;
mod vulkan;
mod wake;
mod wayland;

pub use atlas::AtlasStats;

pub use input::{
    InputSerial, KeyIdentity, KeyLocation, KeySide, KeypadKey, LogicalKey, ModifierKind,
    ModifierMask, SeatToken, SerialKind, key_identity_from_keysym, keypad_key_from_keysym,
    keysym_character, logical_key_from_keysym,
};
pub use model::{
    ClipboardEvent, CommittedFrameKey, FrameKey, GfxCommand, GlyphInstance, GlyphPlacement,
    GlyphRenderMode, KeyInput, KeyState, LinearColor, LogicalSize, ModifiersState, PixelSize,
    PlatformEvent, PointerCursor, PointerInput, PointerKind, RectangleInstance, RenderOutcome,
    RenderScene, Scale120, SceneData, SelectionTarget, TextInputContext, TextInputEvent,
    TextInputPurpose, TextInputRectangle, WindowState,
};
pub use runtime::{GfxError, GfxInitError, GfxOptions, GfxRuntime, MAX_WINDOW_TITLE_BYTES};
pub use vulkan::{RendererFault, RendererOperation};
pub use wake::{EventWake, WakeError};

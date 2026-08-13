//! Safe application facade for Leyline's Wayland/Vulkan boundary.

mod decor;
mod model;
mod runtime;
mod select;
mod vulkan;
mod wake;
mod wayland;

pub use model::{
    GfxCommand, LinearColor, LogicalSize, PixelSize, PlatformEvent, RectangleInstance,
    RenderOutcome, RenderScene, Scale120, SceneData, WindowState,
};
pub use runtime::{GfxError, GfxInitError, GfxOptions, GfxRuntime};
pub use wake::{EventWake, WakeError};

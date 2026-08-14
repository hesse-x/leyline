use std::{
    os::fd::{BorrowedFd, OwnedFd},
    time::Duration,
};

use crate::{
    GfxCommand, LinearColor, LogicalSize, PlatformEvent, RectangleInstance, RenderOutcome,
    RenderScene, Scale120, SelectionTarget, TextInputRectangle,
    atlas::{AtlasManager, AtlasPreparation},
    model::SceneData,
    vulkan::{RenderStatus, VulkanRenderer},
    wayland::WaylandWindow,
};

pub const MAX_WINDOW_TITLE_BYTES: usize = 1024;

#[derive(Clone, Debug)]
pub struct GfxOptions {
    pub title: String,
    pub default_size: LogicalSize,
    pub clear: LinearColor,
}

impl Default for GfxOptions {
    fn default() -> Self {
        Self {
            title: "Leyline".into(),
            default_size: LogicalSize {
                width: 800,
                height: 500,
            },
            clear: LinearColor::from_srgba8(0x1818_18ff),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GfxInitError {
    #[error("graphics environment error: {0}")]
    Environment(String),
    #[error("Wayland capability error: {0}")]
    Platform(String),
    #[error("Vulkan device is unsuitable: {0}")]
    Device(String),
}

#[derive(Debug, thiserror::Error)]
pub enum GfxError {
    #[error("Wayland runtime error: {0}")]
    Platform(String),
    #[error("Vulkan runtime error: {0}")]
    Renderer(#[from] crate::RendererFault),
    #[error("graphics state invariant failed: {0}")]
    Internal(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceState {
    Ready,
    Suspended,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SwapchainState {
    Ready,
    RecreatePending,
}

pub struct GfxRuntime {
    wayland: WaylandWindow,
    renderer: VulkanRenderer,
    logical_size: LogicalSize,
    scale: Scale120,
    scene: SceneData,
    atlas: AtlasManager,
    pending_atlas: Option<AtlasPreparation>,
    pending_scene: Option<SceneData>,
    glyphs: Vec<crate::GlyphInstance>,
    dirty: bool,
    frame_ready: bool,
    swapchain_state: SwapchainState,
    surface_state: SurfaceState,
    close_requested: bool,
}

impl GfxRuntime {
    /// Connects Wayland, creates a native window and initializes Vulkan 1.3 WSI.
    ///
    /// # Errors
    /// Returns a categorized environment, platform, or device failure.
    pub fn new(options: &GfxOptions) -> Result<Self, GfxInitError> {
        if options.title.contains('\0') || options.title.len() > MAX_WINDOW_TITLE_BYTES {
            return Err(GfxInitError::Platform(
                "initial window title is invalid".into(),
            ));
        }
        let mut wayland = WaylandWindow::connect(&options.title)?;
        for _ in 0..8 {
            wayland.roundtrip().map_err(GfxInitError::Platform)?;
            if wayland.state.configured {
                break;
            }
        }
        if !wayland.state.configured {
            return Err(GfxInitError::Platform(
                "compositor did not send the initial configure".into(),
            ));
        }
        let mut events = Vec::new();
        wayland
            .take_events(&mut events)
            .map_err(GfxInitError::Platform)?;
        let mut logical_size = options.default_size;
        let mut scale = Scale120::ONE;
        for event in events {
            if let PlatformEvent::Configured {
                logical_size: size,
                scale: configured_scale,
                ..
            } = event
            {
                logical_size = size;
                scale = configured_scale;
            }
        }
        let pixels = scale
            .pixels(logical_size)
            .map_err(|error| GfxInitError::Platform(error.to_string()))?;
        let renderer = VulkanRenderer::new(&wayland, pixels)?;
        Ok(Self {
            wayland,
            renderer,
            logical_size,
            scale,
            scene: SceneData {
                clear: options.clear,
                rectangles: test_rectangles(pixels.width, pixels.height),
                glyphs: Vec::new(),
                glyph_assets: Vec::new(),
                source_generation: 0,
                font_generation: 0,
            },
            atlas: AtlasManager::new(),
            pending_atlas: None,
            pending_scene: None,
            glyphs: Vec::new(),
            dirty: true,
            frame_ready: true,
            swapchain_state: SwapchainState::Ready,
            surface_state: SurfaceState::Ready,
            close_requested: false,
        })
    }

    /// Dispatches already-buffered Wayland callbacks and transfers semantic events.
    ///
    /// # Errors
    /// Returns [`GfxError`] on compositor disconnection or protocol failure.
    pub fn dispatch_pending(&mut self, output: &mut Vec<PlatformEvent>) -> Result<(), GfxError> {
        self.wayland
            .dispatch_pending()
            .map_err(GfxError::Platform)?;
        let start = output.len();
        self.wayland
            .take_events(output)
            .map_err(GfxError::Platform)?;
        for event in &output[start..] {
            match event {
                PlatformEvent::CloseRequested => self.close_requested = true,
                PlatformEvent::Configured {
                    logical_size,
                    scale,
                    ..
                } => {
                    let extent_changed = self.logical_size != *logical_size || self.scale != *scale;
                    self.logical_size = *logical_size;
                    self.scale = *scale;
                    if extent_changed {
                        self.dirty = true;
                        self.swapchain_state = SwapchainState::RecreatePending;
                    }
                }
                PlatformEvent::ScaleChanged { scale } => {
                    if self.scale != *scale {
                        self.scale = *scale;
                        self.dirty = true;
                        self.swapchain_state = SwapchainState::RecreatePending;
                    }
                }
                PlatformEvent::FrameReady => self.frame_ready = true,
                PlatformEvent::SurfaceSuspended => self.surface_state = SurfaceState::Suspended,
                PlatformEvent::SurfaceResumed => {
                    self.surface_state = SurfaceState::Ready;
                    self.dirty = true;
                    self.frame_ready = true;
                    self.swapchain_state = SwapchainState::RecreatePending;
                }
                PlatformEvent::KeyboardFocus { .. }
                | PlatformEvent::Key(_)
                | PlatformEvent::ModifiersChanged(_)
                | PlatformEvent::Pointer(_)
                | PlatformEvent::TextInput(_)
                | PlatformEvent::Clipboard(_) => {}
            }
        }
        Ok(())
    }

    /// Applies a safe application command without exposing native handles.
    ///
    /// # Errors
    /// Returns [`GfxError`] when a title contains NUL or violates its bounded length.
    pub fn apply(&mut self, command: GfxCommand) -> Result<(), GfxError> {
        match command {
            GfxCommand::SetTitle(title) => {
                if title.contains('\0') || title.len() > MAX_WINDOW_TITLE_BYTES {
                    return Err(GfxError::Internal("window title is invalid".into()));
                }
                self.wayland.set_title(&title);
            }
            GfxCommand::SetDirty => self.dirty = true,
            GfxCommand::SetScene(scene) => {
                if self.pending_atlas.take().is_some() {
                    self.pending_scene = None;
                    self.renderer.discard_pending_glyphs();
                }
                let prepared = self
                    .atlas
                    .prepare(&scene.glyphs, &scene.glyph_assets)
                    .map_err(|error| GfxError::Internal(error.to_string()))?;
                self.renderer
                    .upload_glyphs(&prepared.uploads, prepared.is_repack())
                    .map_err(GfxError::Renderer)?;
                self.pending_atlas = Some(prepared);
                self.pending_scene = Some(scene);
                self.dirty = true;
            }
            GfxCommand::RequestClose => self.close_requested = true,
        }
        Ok(())
    }

    /// Renders at most one requested frame.
    ///
    /// # Errors
    /// Returns [`GfxError`] for renderer failures; transient GPU readiness is deferred.
    pub fn try_render(&mut self) -> Result<RenderOutcome, GfxError> {
        self.try_render_mode(true)
    }

    /// Presents the existing swapchain while Wayland scales it to pending surface geometry.
    ///
    /// # Errors
    /// Returns [`GfxError`] for renderer or platform failures.
    pub fn try_render_resize_preview(&mut self) -> Result<RenderOutcome, GfxError> {
        self.try_render_mode(false)
    }

    fn try_render_mode(&mut self, allow_recreate: bool) -> Result<RenderOutcome, GfxError> {
        if !self.dirty {
            return Ok(RenderOutcome::Idle);
        }
        if self.surface_state == SurfaceState::Suspended {
            return Ok(RenderOutcome::WaitingForFrame);
        }
        if !self.frame_ready {
            return Ok(RenderOutcome::WaitingForFrame);
        }
        let requested = self
            .scale
            .pixels(self.logical_size)
            .map_err(|error| GfxError::Internal(error.to_string()))?;
        if allow_recreate
            && (self.swapchain_state == SwapchainState::RecreatePending
                || self.renderer.extent() != requested)
        {
            if !self
                .renderer
                .recreate(requested)
                .map_err(GfxError::Renderer)?
            {
                return Ok(RenderOutcome::Deferred);
            }
            self.swapchain_state = SwapchainState::Ready;
        }
        let target = self.renderer.extent();
        let scene_data = self.pending_scene.as_ref().unwrap_or(&self.scene);
        let glyphs = self
            .pending_atlas
            .as_ref()
            .map_or(self.glyphs.as_slice(), AtlasPreparation::instances);
        let scene = RenderScene {
            clear: scene_data.clear,
            viewport: target,
            rectangles: &scene_data.rectangles,
            glyphs,
        };
        let recreate_after_present =
            match self.renderer.render(&scene).map_err(GfxError::Renderer)? {
                RenderStatus::Deferred => return Ok(RenderOutcome::Deferred),
                RenderStatus::OutOfDate => {
                    self.swapchain_state = SwapchainState::RecreatePending;
                    return Ok(RenderOutcome::Deferred);
                }
                RenderStatus::SubmittedOutOfDate => {
                    self.commit_pending_atlas();
                    self.swapchain_state = SwapchainState::RecreatePending;
                    return Ok(RenderOutcome::Deferred);
                }
                RenderStatus::Suboptimal => {
                    self.commit_pending_atlas();
                    true
                }
                RenderStatus::Rendered => {
                    self.commit_pending_atlas();
                    false
                }
            };
        self.wayland.request_frame();
        self.wayland.commit();
        self.wayland.flush().map_err(GfxError::Platform)?;
        self.swapchain_state = if recreate_after_present || target != requested {
            SwapchainState::RecreatePending
        } else {
            SwapchainState::Ready
        };
        self.dirty = recreate_after_present;
        self.frame_ready = false;
        Ok(RenderOutcome::Rendered)
    }

    fn commit_pending_atlas(&mut self) {
        let Some(prepared) = self.pending_atlas.take() else {
            return;
        };
        let committed = self.atlas.commit(prepared);
        if committed.repacked {
            tracing::debug!(
                category = "capacity_pressure",
                operation = "atlas_repack",
                "glyph atlas repacked"
            );
        }
        self.glyphs = committed.instances;
        if let Some(scene) = self.pending_scene.take() {
            self.scene = scene;
        }
    }

    /// Blocks in Wayland's protocol-aware read path until callbacks arrive.
    ///
    /// # Errors
    /// Returns [`GfxError`] on compositor disconnection.
    pub fn wait(&mut self) -> Result<(), GfxError> {
        self.poll_wait(None, None)
    }

    /// Waits for Wayland, a cross-thread eventfd, or a timer in one poll call.
    ///
    /// # Errors
    /// Returns [`GfxError`] on poll or compositor failure.
    pub fn poll_wait(
        &mut self,
        wake: Option<BorrowedFd<'_>>,
        timeout: Option<Duration>,
    ) -> Result<(), GfxError> {
        self.wayland
            .poll_read(wake, timeout)
            .map_err(GfxError::Platform)
    }

    #[must_use]
    pub const fn close_requested(&self) -> bool {
        self.close_requested
    }

    #[must_use]
    pub const fn logical_size(&self) -> LogicalSize {
        self.logical_size
    }

    #[must_use]
    pub const fn scale(&self) -> Scale120 {
        self.scale
    }

    #[must_use]
    pub fn atlas_stats(&self) -> crate::AtlasStats {
        self.atlas.stats()
    }

    #[must_use]
    pub const fn text_input_available(&self) -> bool {
        self.wayland.text_input_available()
    }

    /// Enables text-input and immediately flushes the focus transaction.
    ///
    /// # Errors
    /// Returns a platform error when the compositor connection cannot be flushed.
    pub fn enable_text_input(
        &mut self,
        rectangle: TextInputRectangle,
    ) -> Result<Option<u32>, GfxError> {
        self.wayland
            .enable_text_input(rectangle)
            .map_err(GfxError::Platform)
    }

    /// Updates text-input state and immediately flushes the transaction.
    ///
    /// # Errors
    /// Returns a platform error when the compositor connection cannot be flushed.
    pub fn update_text_input(
        &mut self,
        rectangle: TextInputRectangle,
    ) -> Result<Option<u32>, GfxError> {
        self.wayland
            .update_text_input(rectangle)
            .map_err(GfxError::Platform)
    }

    /// Disables text-input and commits the associated main surface immediately.
    ///
    /// # Errors
    /// Returns a platform error when the compositor connection cannot be flushed.
    pub fn disable_text_input(&mut self) -> Result<Option<u32>, GfxError> {
        self.wayland
            .disable_text_input()
            .map_err(GfxError::Platform)
    }

    /// Commits the latest surface geometry without allocating a new swapchain.
    ///
    /// # Errors
    /// Returns a platform error when the compositor connection cannot be flushed.
    pub fn acknowledge_resize(&mut self) -> Result<(), GfxError> {
        self.wayland.commit();
        self.wayland.flush().map_err(GfxError::Platform)
    }

    /// Publishes a selection and flushes the request to the compositor.
    ///
    /// # Errors
    /// Returns a platform error when the compositor connection cannot be flushed.
    pub fn publish_selection(
        &mut self,
        target: SelectionTarget,
        source: u64,
        serial: crate::InputSerial,
    ) -> Result<bool, GfxError> {
        self.wayland
            .publish_selection(target, source, serial)
            .map_err(GfxError::Platform)
    }

    /// Requests selection contents and flushes before returning the read pipe.
    ///
    /// # Errors
    /// Returns a platform error when the compositor connection cannot be flushed.
    pub fn receive_selection(
        &mut self,
        target: SelectionTarget,
    ) -> Result<Option<OwnedFd>, GfxError> {
        self.wayland
            .receive_selection(target)
            .map_err(GfxError::Platform)
    }

    #[must_use]
    pub const fn retry_delay() -> Duration {
        Duration::from_millis(4)
    }
}

#[allow(clippy::cast_precision_loss)]
fn test_rectangles(width: u32, height: u32) -> Vec<RectangleInstance> {
    let width = width as f32;
    let height = height as f32;
    vec![
        RectangleInstance {
            origin_px: [0.0, 0.0],
            size_px: [width, 4.0],
            color: LinearColor::from_srgba8(0x72a5_80ff),
        },
        RectangleInstance {
            origin_px: [0.0, height - 4.0],
            size_px: [width, 4.0],
            color: LinearColor::from_srgba8(0x72a5_80ff),
        },
        RectangleInstance {
            origin_px: [0.0, 0.0],
            size_px: [4.0, height],
            color: LinearColor::from_srgba8(0x72a5_80ff),
        },
        RectangleInstance {
            origin_px: [width - 4.0, 0.0],
            size_px: [4.0, height],
            color: LinearColor::from_srgba8(0x72a5_80ff),
        },
        RectangleInstance {
            origin_px: [width * 0.5, height * 0.2],
            size_px: [width * 0.5 - 24.0, height * 0.6],
            color: LinearColor::from_srgba8(0x365b_7dff),
        },
    ]
}

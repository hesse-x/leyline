use std::{
    collections::BTreeMap,
    num::{NonZeroU8, NonZeroU64},
    os::fd::BorrowedFd,
    rc::Rc,
    time::{Duration, Instant},
};

use crate::{
    GfxError, GfxOptions, GfxRuntime, PlatformEvent, RenderOutcome, decor::LibdecorContext,
    runtime::PendingGfxRuntime, vulkan::VulkanDeviceContext, wayland::WaylandConnectionHost,
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WindowId(NonZeroU64);

impl WindowId {
    #[must_use]
    pub const fn from_raw(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SurfaceKey {
    pub window: WindowId,
    pub generation: NonZeroU64,
}

#[derive(Debug)]
pub struct RoutedPlatformEvent {
    pub surface: SurfaceKey,
    pub event: PlatformEvent,
}

pub struct GfxHost {
    wayland: Rc<std::cell::RefCell<WaylandConnectionHost>>,
    device: Rc<VulkanDeviceContext>,
    libdecor: Option<Rc<LibdecorContext>>,
    windows: BTreeMap<WindowId, GfxWindow>,
    next_id: Option<NonZeroU64>,
    next_surface_generation: Option<NonZeroU64>,
    max_windows: NonZeroU8,
}

enum GfxWindow {
    Creating {
        surface: SurfaceKey,
        runtime: Box<PendingGfxRuntime>,
        deadline: Instant,
    },
    Ready {
        surface: SurfaceKey,
        runtime: Box<GfxRuntime>,
    },
}

const INITIAL_CONFIGURE_TIMEOUT: Duration = Duration::from_secs(5);

impl GfxHost {
    #[must_use]
    pub fn adopt_initial(window: GfxRuntime, max_windows: NonZeroU8) -> (Self, WindowId) {
        let wayland = window.wayland_host();
        let device = window.device_context();
        let libdecor = window.libdecor_context();
        let id = WindowId(NonZeroU64::MIN);
        let surface = SurfaceKey {
            window: id,
            generation: NonZeroU64::MIN,
        };
        let mut windows = BTreeMap::new();
        windows.insert(
            id,
            GfxWindow::Ready {
                surface,
                runtime: Box::new(window),
            },
        );
        (
            Self {
                wayland,
                device,
                libdecor,
                windows,
                next_id: NonZeroU64::new(2),
                next_surface_generation: NonZeroU64::new(2),
                max_windows,
            },
            id,
        )
    }
    /// Creates one window on the host's existing Wayland connection.
    ///
    /// # Errors
    /// Returns a typed capacity, platform, or renderer initialization failure.
    pub fn create_window(&mut self, options: &GfxOptions) -> Result<WindowId, GfxError> {
        if self.windows.len() >= usize::from(self.max_windows.get()) {
            return Err(GfxError::Capacity(format!(
                "window limit reached ({})",
                self.max_windows
            )));
        }
        let id = WindowId(
            self.next_id
                .ok_or_else(|| GfxError::Capacity("window id space exhausted".into()))?,
        );
        self.next_id = self
            .next_id
            .and_then(|next| next.get().checked_add(1))
            .and_then(NonZeroU64::new);
        let surface = SurfaceKey {
            window: id,
            generation: self
                .next_surface_generation
                .ok_or_else(|| GfxError::Capacity("surface generation space exhausted".into()))?,
        };
        self.next_surface_generation = self
            .next_surface_generation
            .and_then(|next| next.get().checked_add(1))
            .and_then(NonZeroU64::new);
        let runtime = GfxRuntime::begin_on(
            Rc::clone(&self.wayland),
            Rc::clone(&self.device),
            self.libdecor.as_ref().map(Rc::clone),
            options,
        )?;
        self.windows.insert(
            id,
            GfxWindow::Creating {
                surface,
                runtime: Box::new(runtime),
                deadline: Instant::now() + INITIAL_CONFIGURE_TIMEOUT,
            },
        );
        Ok(id)
    }

    /// Removes a window while leaving the shared connection alive.
    ///
    /// # Errors
    /// Returns a typed invariant error for an unknown identity.
    pub fn remove_window(&mut self, id: WindowId) -> Result<(), GfxError> {
        self.windows
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| GfxError::Internal(format!("unknown window {}", id.get())))
    }

    #[must_use]
    pub fn window(&self, id: WindowId) -> Option<&GfxRuntime> {
        match self.windows.get(&id)? {
            GfxWindow::Ready { runtime, .. } => Some(runtime.as_ref()),
            GfxWindow::Creating { .. } => None,
        }
    }

    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut GfxRuntime> {
        match self.windows.get_mut(&id)? {
            GfxWindow::Ready { runtime, .. } => Some(runtime.as_mut()),
            GfxWindow::Creating { .. } => None,
        }
    }

    #[must_use]
    pub fn surface_key(&self, id: WindowId) -> Option<SurfaceKey> {
        match self.windows.get(&id)? {
            GfxWindow::Creating { surface, .. } | GfxWindow::Ready { surface, .. } => {
                Some(*surface)
            }
        }
    }

    #[must_use]
    pub fn accepts_surface(&self, surface: SurfaceKey) -> bool {
        self.surface_key(surface.window) == Some(surface)
    }

    #[must_use]
    pub fn is_creating(&self, id: WindowId) -> bool {
        matches!(self.windows.get(&id), Some(GfxWindow::Creating { .. }))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Dispatches callbacks for every window queue and tags each emitted event.
    ///
    /// # Errors
    /// Returns a connection or window runtime failure.
    pub fn dispatch_pending(
        &mut self,
        output: &mut Vec<RoutedPlatformEvent>,
    ) -> Result<(), GfxError> {
        let mut events = Vec::new();
        let mut became_ready = Vec::new();
        for (id, window) in &mut self.windows {
            events.clear();
            match window {
                GfxWindow::Creating {
                    runtime, surface, ..
                } => {
                    runtime.dispatch_pending(&mut events)?;
                    if runtime.is_configured() {
                        became_ready.push(*id);
                    }
                    output.extend(events.drain(..).map(|event| RoutedPlatformEvent {
                        surface: *surface,
                        event,
                    }));
                }
                GfxWindow::Ready { surface, runtime } => {
                    runtime.dispatch_pending(&mut events)?;
                    output.extend(events.drain(..).map(|event| RoutedPlatformEvent {
                        surface: *surface,
                        event,
                    }));
                }
            }
        }
        for id in became_ready {
            let Some(GfxWindow::Creating {
                surface,
                runtime: window,
                ..
            }) = self.windows.remove(&id)
            else {
                return Err(GfxError::Internal(
                    "creating window disappeared during renderer preparation".into(),
                ));
            };
            self.windows.insert(
                id,
                GfxWindow::Ready {
                    surface,
                    runtime: Box::new((*window).finish()?),
                },
            );
        }
        Ok(())
    }

    #[must_use]
    pub fn next_creation_deadline(&self) -> Option<Instant> {
        self.windows
            .values()
            .filter_map(|window| match window {
                GfxWindow::Creating { deadline, .. } => Some(*deadline),
                GfxWindow::Ready { .. } => None,
            })
            .min()
    }

    pub fn expire_creating(&mut self, now: Instant, expired: &mut Vec<WindowId>) {
        self.windows.retain(|id, window| {
            let keep = !matches!(window, GfxWindow::Creating { deadline, .. } if *deadline <= now);
            if !keep {
                expired.push(*id);
            }
            keep
        });
    }

    /// Renders each dirty, frame-ready window at most once.
    ///
    /// # Errors
    /// Returns the first platform or renderer failure.
    pub fn try_render(
        &mut self,
        output: &mut Vec<(WindowId, RenderOutcome)>,
    ) -> Result<(), GfxError> {
        for (id, window) in &mut self.windows {
            if let GfxWindow::Ready { runtime, .. } = window {
                output.push((*id, runtime.try_render()?));
            }
        }
        Ok(())
    }

    /// Waits once on the shared Wayland connection through an arbitrary window queue.
    ///
    /// # Errors
    /// Returns an error when no window exists or polling the compositor fails.
    pub fn poll_wait(
        &mut self,
        wake: Option<BorrowedFd<'_>>,
        timeout: Option<Duration>,
    ) -> Result<(), GfxError> {
        match self
            .windows
            .first_entry()
            .ok_or_else(|| GfxError::Internal("cannot poll an empty graphics host".into()))?
            .get_mut()
        {
            GfxWindow::Creating {
                runtime: window, ..
            } => window.poll_wait(wake, timeout),
            GfxWindow::Ready { runtime, .. } => runtime.poll_wait(wake, timeout),
        }
    }
}

use std::{
    collections::HashMap,
    num::NonZeroU64,
    time::{Duration, Instant},
};

use crate::tab::SessionId;

pub use leyline_gfx::{SurfaceKey, WindowId};

#[derive(Clone, Debug)]
pub struct WindowIdAllocator {
    next: Option<NonZeroU64>,
}

impl Default for WindowIdAllocator {
    fn default() -> Self {
        Self {
            next: Some(NonZeroU64::MIN),
        }
    }
}

impl WindowIdAllocator {
    /// Allocates the next process-unique window identity.
    ///
    /// # Errors
    /// Returns [`WindowError::WindowIdExhausted`] after the final `u64` identity.
    pub fn allocate(&mut self) -> Result<WindowId, WindowError> {
        let id = WindowId::from_raw(self.next.ok_or(WindowError::WindowIdExhausted)?.get())
            .ok_or(WindowError::WindowIdExhausted)?;
        self.next = self
            .next
            .and_then(|next| next.get().checked_add(1))
            .and_then(NonZeroU64::new);
        Ok(id)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DesiredWindowState {
    pub maximized: bool,
    pub fullscreen: bool,
}

impl DesiredWindowState {
    #[must_use]
    pub fn toggle_fullscreen(mut self) -> Self {
        self.fullscreen = !self.fullscreen;
        self
    }

    #[must_use]
    pub fn toggle_maximized(mut self) -> Self {
        self.maximized = !self.maximized;
        self
    }

    #[must_use]
    pub const fn restore() -> Self {
        Self {
            maximized: false,
            fullscreen: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowStateRequest {
    SetMaximized(bool),
    SetFullscreen(bool),
    Restore,
}

#[derive(Clone, Copy, Debug)]
pub struct PendingStateRequest {
    pub generation: NonZeroU64,
    pub deadline: Instant,
}

#[derive(Clone, Copy, Debug)]
pub struct WindowStateController {
    desired: DesiredWindowState,
    effective: leyline_gfx::WindowState,
    pending: Option<PendingStateRequest>,
    next_generation: NonZeroU64,
}

impl WindowStateController {
    #[must_use]
    pub fn new(desired: DesiredWindowState) -> Self {
        Self {
            desired,
            effective: leyline_gfx::WindowState::default(),
            pending: None,
            next_generation: NonZeroU64::MIN,
        }
    }

    #[must_use]
    pub const fn desired(&self) -> DesiredWindowState {
        self.desired
    }

    #[must_use]
    pub const fn effective(&self) -> leyline_gfx::WindowState {
        self.effective
    }

    #[must_use]
    pub const fn pending(&self) -> Option<PendingStateRequest> {
        self.pending
    }

    /// Starts or replaces a bounded compositor-authoritative state request.
    ///
    /// # Errors
    /// Returns [`WindowError::StateGenerationExhausted`] if its generation cannot advance.
    pub fn request(
        &mut self,
        request: WindowStateRequest,
        now: Instant,
        timeout: Duration,
    ) -> Result<DesiredWindowState, WindowError> {
        self.desired = match request {
            WindowStateRequest::SetMaximized(value) => DesiredWindowState {
                maximized: value,
                ..self.desired
            },
            WindowStateRequest::SetFullscreen(value) => DesiredWindowState {
                fullscreen: value,
                ..self.desired
            },
            WindowStateRequest::Restore => DesiredWindowState::restore(),
        };
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(WindowError::StateGenerationExhausted)?;
        self.pending = Some(PendingStateRequest {
            generation,
            deadline: now + timeout,
        });
        Ok(self.desired)
    }

    pub fn configured(&mut self, effective: leyline_gfx::WindowState) {
        self.effective = effective;
        if self.effective.maximized == self.desired.maximized
            && self.effective.fullscreen == self.desired.fullscreen
        {
            self.pending = None;
        }
    }

    pub fn expire(&mut self, now: Instant) -> bool {
        if self.pending.is_some_and(|pending| now >= pending.deadline) {
            self.pending = None;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLocation {
    Active { window: WindowId },
    Moving { token: NonZeroU64, source: WindowId },
    Closing { former_window: WindowId },
}

#[derive(Clone, Debug)]
pub struct WindowState {
    pub id: WindowId,
    pub surface: SurfaceKey,
    pub tabs: Vec<SessionId>,
    pub active: SessionId,
    pub desired_state: DesiredWindowState,
    pub effective_state: leyline_gfx::WindowState,
}

#[derive(Default)]
pub struct WindowManager {
    windows: HashMap<WindowId, WindowState>,
    locations: HashMap<SessionId, SessionLocation>,
}

#[allow(clippy::missing_errors_doc)]
impl WindowManager {
    pub fn insert_ready(&mut self, state: WindowState) -> Result<(), WindowError> {
        validate_ready(&state)?;
        if self.windows.contains_key(&state.id) {
            return Err(WindowError::DuplicateWindow(state.id));
        }
        if state.tabs.iter().any(|id| self.locations.contains_key(id)) {
            return Err(WindowError::SessionAlreadyOwned);
        }
        for id in &state.tabs {
            self.locations
                .insert(*id, SessionLocation::Active { window: state.id });
        }
        self.windows.insert(state.id, state);
        Ok(())
    }

    #[must_use]
    pub fn route_surface(&self, surface: SurfaceKey) -> Option<WindowId> {
        self.windows
            .get(&surface.window)
            .filter(|window| window.surface == surface)
            .map(|window| window.id)
    }

    #[must_use]
    pub fn location(&self, session: SessionId) -> Option<SessionLocation> {
        self.locations.get(&session).copied()
    }

    pub fn reorder(
        &mut self,
        window: WindowId,
        session: SessionId,
        target: usize,
    ) -> Result<crate::tab::ReorderOutcome, WindowError> {
        let state = self
            .windows
            .get_mut(&window)
            .ok_or(WindowError::UnknownWindow(window))?;
        let from = state
            .tabs
            .iter()
            .position(|id| *id == session)
            .ok_or(WindowError::UnknownSession(session))?;
        let to = target.min(state.tabs.len().saturating_sub(1));
        if from == to {
            return Ok(crate::tab::ReorderOutcome::Unchanged);
        }
        let id = state.tabs.remove(from);
        state.tabs.insert(to, id);
        Ok(crate::tab::ReorderOutcome::Changed { from, to })
    }

    pub fn close(&mut self, window: WindowId) -> Result<Vec<SessionId>, WindowError> {
        let state = self
            .windows
            .remove(&window)
            .ok_or(WindowError::UnknownWindow(window))?;
        for id in &state.tabs {
            self.locations.insert(
                *id,
                SessionLocation::Closing {
                    former_window: window,
                },
            );
        }
        Ok(state.tabs)
    }

    pub fn move_tab_to_ready_window(
        &mut self,
        source: WindowId,
        target: WindowState,
        session: SessionId,
    ) -> Result<bool, WindowError> {
        validate_ready(&target)?;
        if target.tabs != [session] || target.active != session {
            return Err(WindowError::InvalidReadyWindow);
        }
        if self.windows.contains_key(&target.id) {
            return Err(WindowError::DuplicateWindow(target.id));
        }
        if self.location(session) != Some(SessionLocation::Active { window: source }) {
            return Err(WindowError::UnknownSession(session));
        }
        let source_state = self
            .windows
            .get_mut(&source)
            .ok_or(WindowError::UnknownWindow(source))?;
        let index = source_state
            .tabs
            .iter()
            .position(|id| *id == session)
            .ok_or(WindowError::UnknownSession(session))?;
        source_state.tabs.remove(index);
        let source_closed = source_state.tabs.is_empty();
        if !source_closed && source_state.active == session {
            source_state.active = source_state.tabs[index.min(source_state.tabs.len() - 1)];
        }
        if source_closed {
            self.windows.remove(&source);
        }
        self.locations
            .insert(session, SessionLocation::Active { window: target.id });
        self.windows.insert(target.id, target);
        Ok(source_closed)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

fn validate_ready(state: &WindowState) -> Result<(), WindowError> {
    if state.tabs.is_empty() || !state.tabs.contains(&state.active) {
        return Err(WindowError::InvalidReadyWindow);
    }
    let mut unique = state.tabs.clone();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != state.tabs.len() {
        return Err(WindowError::InvalidReadyWindow);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WindowError {
    #[error("window id space exhausted")]
    WindowIdExhausted,
    #[error("unknown window {0:?}")]
    UnknownWindow(WindowId),
    #[error("duplicate window {0:?}")]
    DuplicateWindow(WindowId),
    #[error("unknown session {0:?}")]
    UnknownSession(SessionId),
    #[error("session is already owned")]
    SessionAlreadyOwned,
    #[error("ready window must contain one unique active session")]
    InvalidReadyWindow,
    #[error("window state request generation exhausted")]
    StateGenerationExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_ids_include_last_value_then_report_exhaustion() {
        let mut allocator = WindowIdAllocator {
            next: NonZeroU64::new(u64::MAX),
        };
        assert_eq!(allocator.allocate().unwrap().get(), u64::MAX);
        assert_eq!(allocator.allocate(), Err(WindowError::WindowIdExhausted));
    }

    fn session(raw: u64) -> SessionId {
        SessionId::from_raw(raw)
    }

    fn ready(id: WindowId, sessions: &[u64], active: u64) -> WindowState {
        WindowState {
            id,
            surface: SurfaceKey {
                window: id,
                generation: NonZeroU64::MIN,
            },
            tabs: sessions.iter().copied().map(session).collect(),
            active: session(active),
            desired_state: DesiredWindowState::default(),
            effective_state: leyline_gfx::WindowState::default(),
        }
    }

    #[test]
    fn stale_surface_generation_is_rejected() {
        let mut ids = WindowIdAllocator::default();
        let id = ids.allocate().unwrap();
        let surface = SurfaceKey {
            window: id,
            generation: NonZeroU64::MIN,
        };
        let mut manager = WindowManager::default();
        manager
            .insert_ready(WindowState {
                id,
                surface,
                tabs: vec![session(1)],
                active: session(1),
                desired_state: DesiredWindowState::default(),
                effective_state: leyline_gfx::WindowState::default(),
            })
            .unwrap();
        assert_eq!(manager.route_surface(surface), Some(id));
        assert_eq!(
            manager.route_surface(SurfaceKey {
                generation: NonZeroU64::new(2).unwrap(),
                ..surface
            }),
            None
        );
    }

    #[test]
    fn ready_windows_enforce_unique_session_ownership() {
        let mut ids = WindowIdAllocator::default();
        let first = ids.allocate().unwrap();
        let second = ids.allocate().unwrap();
        let mut manager = WindowManager::default();
        for id in [first, second] {
            let result = manager.insert_ready(WindowState {
                id,
                surface: SurfaceKey {
                    window: id,
                    generation: NonZeroU64::MIN,
                },
                tabs: vec![session(1)],
                active: session(1),
                desired_state: DesiredWindowState::default(),
                effective_state: leyline_gfx::WindowState::default(),
            });
            if id == first {
                result.unwrap();
            } else {
                assert_eq!(result, Err(WindowError::SessionAlreadyOwned));
            }
        }
    }

    #[test]
    fn state_requests_wait_for_matching_configure_or_deadline() {
        let now = Instant::now();
        let mut state = WindowStateController::new(DesiredWindowState::default());
        state
            .request(
                WindowStateRequest::SetMaximized(true),
                now,
                Duration::from_secs(1),
            )
            .unwrap();
        state.configured(leyline_gfx::WindowState {
            activated: true,
            ..leyline_gfx::WindowState::default()
        });
        assert!(state.pending().is_some());
        state.configured(leyline_gfx::WindowState {
            maximized: true,
            ..leyline_gfx::WindowState::default()
        });
        assert!(state.pending().is_none());

        state
            .request(
                WindowStateRequest::SetFullscreen(true),
                now,
                Duration::from_secs(1),
            )
            .unwrap();
        assert!(!state.expire(now + Duration::from_millis(999)));
        assert!(state.expire(now + Duration::from_secs(1)));
    }

    #[test]
    fn fullscreen_and_maximized_desires_are_independent() {
        let now = Instant::now();
        let mut state = WindowStateController::new(DesiredWindowState::default());
        state
            .request(
                WindowStateRequest::SetMaximized(true),
                now,
                Duration::from_secs(1),
            )
            .unwrap();
        let desired = state
            .request(
                WindowStateRequest::SetFullscreen(true),
                now,
                Duration::from_secs(1),
            )
            .unwrap();
        assert!(desired.maximized && desired.fullscreen);
        assert_eq!(
            state
                .request(WindowStateRequest::Restore, now, Duration::from_secs(1))
                .unwrap(),
            DesiredWindowState::restore()
        );
    }

    #[test]
    fn closing_one_window_keeps_other_windows_and_marks_only_its_sessions_closing() {
        let mut ids = WindowIdAllocator::default();
        let first = ids.allocate().unwrap();
        let second = ids.allocate().unwrap();
        let mut manager = WindowManager::default();
        manager.insert_ready(ready(first, &[1], 1)).unwrap();
        manager.insert_ready(ready(second, &[2], 2)).unwrap();

        assert_eq!(manager.close(second).unwrap(), [session(2)]);
        assert_eq!(
            manager.location(session(1)),
            Some(SessionLocation::Active { window: first })
        );
        assert_eq!(
            manager.location(session(2)),
            Some(SessionLocation::Closing {
                former_window: second
            })
        );
        assert!(!manager.is_empty());

        assert_eq!(manager.close(first).unwrap(), [session(1)]);
        assert!(manager.is_empty());
    }

    #[test]
    fn moving_last_tab_closes_source_and_preserves_session_identity() {
        let mut ids = WindowIdAllocator::default();
        let source = ids.allocate().unwrap();
        let target = ids.allocate().unwrap();
        let mut manager = WindowManager::default();
        manager.insert_ready(ready(source, &[7], 7)).unwrap();

        assert!(
            manager
                .move_tab_to_ready_window(source, ready(target, &[7], 7), session(7))
                .unwrap()
        );
        assert_eq!(
            manager.location(session(7)),
            Some(SessionLocation::Active { window: target })
        );
        assert_eq!(manager.route_surface(ready(source, &[7], 7).surface), None);
        assert_eq!(
            manager.route_surface(ready(target, &[7], 7).surface),
            Some(target)
        );
    }

    #[test]
    fn rejected_move_leaves_source_ownership_unchanged() {
        let mut ids = WindowIdAllocator::default();
        let source = ids.allocate().unwrap();
        let target = ids.allocate().unwrap();
        let mut manager = WindowManager::default();
        manager.insert_ready(ready(source, &[3, 4], 3)).unwrap();

        assert_eq!(
            manager.move_tab_to_ready_window(source, ready(target, &[9], 9), session(3)),
            Err(WindowError::InvalidReadyWindow)
        );
        assert_eq!(
            manager.location(session(3)),
            Some(SessionLocation::Active { window: source })
        );
        assert_eq!(
            manager.location(session(4)),
            Some(SessionLocation::Active { window: source })
        );
        assert_eq!(
            manager.route_surface(ready(source, &[3, 4], 3).surface),
            Some(source)
        );
    }

    #[test]
    fn deterministic_operation_sequences_preserve_global_ownership() {
        for seed in 1_u64..=128 {
            let mut random = seed;
            let mut ids = WindowIdAllocator::default();
            let mut manager = WindowManager::default();
            for sessions in [[1, 2], [3, 4], [5, 6]] {
                let id = ids.allocate().unwrap();
                manager
                    .insert_ready(ready(id, &sessions, sessions[0]))
                    .unwrap();
            }

            for _ in 0..64 {
                random = random
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let mut windows = manager.windows.keys().copied().collect::<Vec<_>>();
                windows.sort_unstable();
                if windows.is_empty() {
                    break;
                }
                let bounded = |value: u64, len: usize| {
                    usize::try_from(value % u64::try_from(len).unwrap()).unwrap()
                };
                let source = windows[bounded(random, windows.len())];
                let state = manager.windows.get(&source).unwrap().clone();
                match (random >> 16) % 3 {
                    0 => {
                        let session = state.tabs[bounded(random, state.tabs.len())];
                        manager
                            .reorder(source, session, bounded(random >> 32, state.tabs.len()))
                            .unwrap();
                    }
                    1 if manager.windows.len() < 16 => {
                        let session = state.tabs[bounded(random, state.tabs.len())];
                        let target = ids.allocate().unwrap();
                        manager
                            .move_tab_to_ready_window(
                                source,
                                ready(target, &[session.get()], session.get()),
                                session,
                            )
                            .unwrap();
                    }
                    2 if manager.windows.len() > 1 => {
                        manager.close(source).unwrap();
                    }
                    _ => {}
                }
                assert_manager_invariants(&manager);
            }
        }
    }

    fn assert_manager_invariants(manager: &WindowManager) {
        let mut active = HashMap::new();
        for (window, state) in &manager.windows {
            validate_ready(state).unwrap();
            for session in &state.tabs {
                assert_eq!(
                    active.insert(*session, *window),
                    None,
                    "session appears in multiple windows"
                );
                assert_eq!(
                    manager.location(*session),
                    Some(SessionLocation::Active { window: *window })
                );
            }
        }
        for (session, location) in &manager.locations {
            match location {
                SessionLocation::Active { window } => {
                    assert_eq!(active.get(session), Some(window));
                }
                SessionLocation::Moving { .. } | SessionLocation::Closing { .. } => {
                    assert!(!active.contains_key(session));
                }
            }
        }
    }
}

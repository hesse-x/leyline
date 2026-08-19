use std::num::{NonZeroU8, NonZeroU64};

use crate::{
    app::runtime::AppRuntime,
    session::TerminalSession,
    terminal::cwd::{CwdRejectReason, CwdReport, LocalCwdHint, LocalIdentity, validate_report},
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionId(NonZeroU64);

impl SessionId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    #[cfg(test)]
    pub(crate) fn from_raw(value: u64) -> Self {
        Self(NonZeroU64::new(value).expect("test session id must be non-zero"))
    }
}

#[derive(Clone, Debug)]
pub struct SessionIdAllocator {
    next: Option<NonZeroU64>,
}

impl Default for SessionIdAllocator {
    fn default() -> Self {
        Self {
            next: Some(NonZeroU64::MIN),
        }
    }
}

impl SessionIdAllocator {
    /// Allocates the next process-unique session identity.
    ///
    /// # Errors
    /// Returns [`TabError::SessionIdExhausted`] after the final `u64` identity.
    pub fn allocate(&mut self) -> Result<SessionId, TabError> {
        let id = SessionId(self.next.ok_or(TabError::SessionIdExhausted)?);
        self.next = self
            .next
            .and_then(|next| next.get().checked_add(1))
            .and_then(NonZeroU64::new);
        Ok(id)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BellGeneration(NonZeroU64);

impl BellGeneration {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

pub struct TabEntry {
    pub id: SessionId,
    pub session: TerminalSession,
    pub runtime: AppRuntime,
    pub title: String,
    pub title_source: TabTitleSource,
    pub cwd_hint: Option<LocalCwdHint>,
    pub last_cwd_reject: Option<CwdRejectReason>,
    pub unread: bool,
    pub attention: bool,
    pub bell_muted: bool,
    active_bell_generation: Option<BellGeneration>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabTitleSource {
    Default,
    Explicit,
}

pub struct ClosingTab {
    pub entry: TabEntry,
}

pub struct TabManager {
    tabs: Vec<TabEntry>,
    closing: Vec<ClosingTab>,
    active_id: Option<SessionId>,
    next_bell_generation: NonZeroU64,
    drain_cursor: Option<SessionId>,
    max_count: NonZeroU8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    #[must_use]
    pub fn contains(self, point: [u32; 2]) -> bool {
        point[0] >= self.x
            && point[1] >= self.y
            && point[0] < self.x.saturating_add(self.width)
            && point[1] < self.y.saturating_add(self.height)
    }
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct TabBarItem {
    pub id: SessionId,
    pub title: String,
    pub rect: PixelRect,
    pub close_rect: Option<PixelRect>,
    pub active: bool,
    pub unread: bool,
    pub attention: bool,
    pub bell_muted: bool,
    pub drag_offset_x: i32,
}

#[derive(Clone, Debug, Default)]
pub struct TabBarPresentation {
    pub bar: Option<PixelRect>,
    pub items: Vec<TabBarItem>,
    pub max_offset: u32,
    pub offset: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabDragPhase {
    Armed,
    Dragging,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabDrag {
    pub session: SessionId,
    pub press_serial: leyline_gfx::InputSerial,
    pub origin: [f64; 2],
    pub current: [f64; 2],
    pub proposed_index: usize,
    pub phase: TabDragPhase,
}

impl TabBarPresentation {
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn layout(
        manager: &TabManager,
        viewport_width: u32,
        scale_120: u32,
        config: &crate::config::TabsConfig,
        requested_offset: u32,
    ) -> Self {
        let visible = match config.visibility {
            crate::config::TabBarVisibility::Always => !manager.tabs.is_empty(),
            crate::config::TabBarVisibility::Multiple => manager.tabs.len() >= 2,
            crate::config::TabBarVisibility::Never => false,
        };
        if !visible {
            return Self::default();
        }
        let scaled = |value: u16| {
            u32::from(value)
                .saturating_mul(scale_120)
                .saturating_add(119)
                / 120
        };
        let height = scaled(config.bar_height).max(1);
        let min_width = scaled(config.min_width).max(1);
        let count = u32::try_from(manager.tabs.len()).unwrap_or(u32::MAX).max(1);
        let fills_viewport = min_width.saturating_mul(count) <= viewport_width;
        let item_bounds = |index: u32| {
            if fills_viewport {
                let start =
                    u64::from(viewport_width).saturating_mul(u64::from(index)) / u64::from(count);
                let end = u64::from(viewport_width)
                    .saturating_mul(u64::from(index.saturating_add(1)))
                    / u64::from(count);
                (
                    u32::try_from(start).unwrap_or(u32::MAX),
                    u32::try_from(end).unwrap_or(u32::MAX),
                )
            } else {
                let start = index.saturating_mul(min_width);
                (start, start.saturating_add(min_width))
            }
        };
        let total_width = if fills_viewport {
            viewport_width
        } else {
            min_width.saturating_mul(count)
        };
        let max_offset = total_width.saturating_sub(viewport_width);
        let active_index = u32::try_from(manager.active_index().unwrap_or(0)).unwrap_or(u32::MAX);
        let (active_start, active_end) = item_bounds(active_index);
        let mut offset = requested_offset.min(max_offset);
        if active_start < offset {
            offset = active_start;
        }
        if active_end > offset.saturating_add(viewport_width) {
            offset = active_end.saturating_sub(viewport_width).min(max_offset);
        }
        let items = manager
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| {
                let (logical_start, logical_end) = item_bounds(u32::try_from(index).ok()?);
                let start = logical_start.max(offset);
                let end = logical_end.min(offset.saturating_add(viewport_width));
                if start >= end {
                    return None;
                }
                let x = start.saturating_sub(offset);
                let width = end.saturating_sub(start);
                let rect = PixelRect {
                    x,
                    y: 0,
                    width,
                    height,
                };
                // Keep the visible close mark small while giving it a forgiving click target.
                let close_hit_size = height.saturating_mul(3).div_ceil(4).max(16).min(height);
                let close_rect = (config.show_close_button
                    && width >= close_hit_size.saturating_add(32))
                .then_some(PixelRect {
                    x: x.saturating_add(width)
                        .saturating_sub(close_hit_size.saturating_add(4)),
                    y: (height - close_hit_size) / 2,
                    width: close_hit_size,
                    height: close_hit_size,
                });
                Some(TabBarItem {
                    id: tab.id,
                    title: elide_title(&tab.title),
                    rect,
                    close_rect,
                    active: Some(tab.id) == manager.active_id,
                    unread: tab.unread,
                    attention: tab.attention,
                    bell_muted: tab.bell_muted,
                    drag_offset_x: 0,
                })
            })
            .collect();
        Self {
            bar: Some(PixelRect {
                x: 0,
                y: 0,
                width: viewport_width,
                height,
            }),
            items,
            max_offset,
            offset,
        }
    }

    #[must_use]
    pub fn hit(&self, point: [u32; 2]) -> Option<(SessionId, bool)> {
        self.items
            .iter()
            .find(|item| item.rect.contains(point))
            .map(|item| {
                (
                    item.id,
                    item.close_rect.is_some_and(|rect| rect.contains(point)),
                )
            })
    }

    #[must_use]
    pub fn proposed_index(&self, manager: &TabManager, x: u32) -> Option<usize> {
        let item = self
            .items
            .iter()
            .find(|item| x < item.rect.x.saturating_add(item.rect.width / 2))
            .or_else(|| self.items.last())?;
        manager.tabs.iter().position(|tab| tab.id == item.id)
    }

    pub fn apply_drag_preview(&mut self, session: SessionId, offset_x: i32) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == session) {
            item.drag_offset_x = offset_x;
        }
    }
}

fn elide_title(title: &str) -> String {
    const MAX_CHARS: usize = 64;
    let mut chars = title.chars();
    let mut result = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        result.push_str("...");
    }
    result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Activation {
    Changed { from: SessionId, to: SessionId },
    Unchanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReorderOutcome {
    Unchanged,
    Changed { from: usize, to: usize },
}

#[allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]
impl TabManager {
    #[must_use]
    pub fn bootstrap(session: TerminalSession, runtime: AppRuntime, max_count: NonZeroU8) -> Self {
        let id = SessionId(NonZeroU64::MIN);
        Self::bootstrap_with_id(id, session, runtime, max_count)
    }

    #[must_use]
    pub fn bootstrap_with_id(
        id: SessionId,
        session: TerminalSession,
        runtime: AppRuntime,
        max_count: NonZeroU8,
    ) -> Self {
        Self {
            tabs: vec![TabEntry {
                id,
                session,
                runtime,
                title: "Shell".into(),
                title_source: TabTitleSource::Default,
                cwd_hint: None,
                last_cwd_reject: None,
                unread: false,
                attention: false,
                bell_muted: false,
                active_bell_generation: None,
            }],
            closing: Vec::new(),
            active_id: Some(id),
            next_bell_generation: NonZeroU64::MIN,
            drain_cursor: None,
            max_count,
        }
    }

    #[must_use]
    pub fn bootstrap_entry(entry: TabEntry, max_count: NonZeroU8) -> Self {
        let id = entry.id;
        Self {
            tabs: vec![entry],
            closing: Vec::new(),
            active_id: Some(id),
            next_bell_generation: NonZeroU64::MIN,
            drain_cursor: None,
            max_count,
        }
    }

    pub fn push(
        &mut self,
        session: TerminalSession,
        runtime: AppRuntime,
        title: String,
    ) -> Result<SessionId, TabError> {
        if self.tabs.len() + self.closing.len() >= usize::from(self.max_count.get()) {
            return Err(TabError::LimitReached {
                limit: usize::from(self.max_count.get()),
            });
        }
        let next = self
            .tabs
            .iter()
            .chain(self.closing.iter().map(|tab| &tab.entry))
            .map(|tab| tab.id.get())
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(TabError::SessionIdExhausted)?;
        self.push_with_id(SessionId(next), session, runtime, title)
    }

    pub fn push_with_id(
        &mut self,
        id: SessionId,
        session: TerminalSession,
        runtime: AppRuntime,
        title: String,
    ) -> Result<SessionId, TabError> {
        if self.tabs.len() + self.closing.len() >= usize::from(self.max_count.get()) {
            return Err(TabError::LimitReached {
                limit: usize::from(self.max_count.get()),
            });
        }
        if self.tabs.iter().any(|tab| tab.id == id)
            || self.closing.iter().any(|tab| tab.entry.id == id)
        {
            return Err(TabError::DuplicateSession(id));
        }
        self.tabs.push(TabEntry {
            id,
            session,
            runtime,
            title,
            title_source: TabTitleSource::Default,
            cwd_hint: None,
            last_cwd_reject: None,
            unread: false,
            attention: false,
            bell_muted: false,
            active_bell_generation: None,
        });
        self.active_id = Some(id);
        self.assert_invariants();
        Ok(id)
    }

    #[must_use]
    pub fn active_id(&self) -> Option<SessionId> {
        self.active_id
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }
    #[must_use]
    pub fn total_len(&self) -> usize {
        self.tabs.len() + self.closing.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
    #[must_use]
    pub fn closing_is_empty(&self) -> bool {
        self.closing.is_empty()
    }
    #[must_use]
    pub fn has_capacity(&self) -> bool {
        self.tabs.len() + self.closing.len() < usize::from(self.max_count.get())
    }
    #[must_use]
    pub fn tabs(&self) -> &[TabEntry] {
        &self.tabs
    }
    pub fn tabs_mut(&mut self) -> &mut [TabEntry] {
        &mut self.tabs
    }
    pub fn closing_mut(&mut self) -> &mut Vec<ClosingTab> {
        &mut self.closing
    }
    #[must_use]
    pub fn closing(&self) -> &[ClosingTab] {
        &self.closing
    }

    pub fn poll_closing(
        &mut self,
        now: std::time::Instant,
    ) -> Result<(), crate::session::SessionError> {
        let mut index = 0;
        while index < self.closing.len() {
            self.closing[index].entry.runtime.inbox().drain_round(drop);
            let done = !matches!(
                self.closing[index].entry.session.poll_shutdown(now)?,
                crate::session::ShutdownPoll::Pending
            );
            if done {
                let id = self.closing[index].entry.id;
                self.closing.swap_remove(index);
                tracing::debug!(
                    category = "tab_shutdown_complete",
                    session_id = id.get(),
                    "tab shutdown complete"
                );
            } else {
                index += 1;
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn next_closing_deadline(&self) -> Option<std::time::Instant> {
        self.closing
            .iter()
            .filter_map(|tab| tab.entry.session.shutdown_deadline())
            .min()
    }

    #[must_use]
    pub fn active(&self) -> Option<&TabEntry> {
        let id = self.active_id?;
        self.tabs.iter().find(|tab| tab.id == id)
    }
    pub fn active_mut(&mut self) -> Option<&mut TabEntry> {
        let id = self.active_id?;
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    pub fn activate(&mut self, target: SessionId) -> Result<Activation, TabError> {
        if !self.tabs.iter().any(|tab| tab.id == target) {
            return Err(TabError::UnknownSession(target));
        }
        let from = self.active_id.ok_or(TabError::UnknownSession(target))?;
        if from == target {
            return Ok(Activation::Unchanged);
        }
        self.active_id = Some(target);
        self.assert_invariants();
        Ok(Activation::Changed { from, to: target })
    }

    pub fn activate_relative(&mut self, delta: i8) -> Activation {
        if self.tabs.len() < 2 {
            return Activation::Unchanged;
        }
        let current = self.active_index().expect("active tab invariant");
        let len = self.tabs.len();
        let target = if delta < 0 {
            (current + len - 1) % len
        } else {
            (current + 1) % len
        };
        self.activate(self.tabs[target].id).expect("target exists")
    }

    pub fn activate_ordinal(&mut self, ordinal: u8) -> Activation {
        let Some(index) = usize::from(ordinal).checked_sub(1) else {
            return Activation::Unchanged;
        };
        let Some(tab) = self.tabs.get(index) else {
            return Activation::Unchanged;
        };
        self.activate(tab.id).expect("target exists")
    }

    pub fn reorder(
        &mut self,
        session: SessionId,
        target_index: usize,
    ) -> Result<ReorderOutcome, TabError> {
        let from = self
            .tabs
            .iter()
            .position(|tab| tab.id == session)
            .ok_or(TabError::UnknownSession(session))?;
        let to = target_index.min(self.tabs.len().saturating_sub(1));
        if from == to {
            return Ok(ReorderOutcome::Unchanged);
        }
        let entry = self.tabs.remove(from);
        self.tabs.insert(to, entry);
        // The cursor is identity-based, so keeping it preserves fair rotation across reorders.
        self.assert_invariants();
        Ok(ReorderOutcome::Changed { from, to })
    }

    pub fn move_active(&mut self, delta: i8) -> Result<ReorderOutcome, TabError> {
        let id = self.active_id.ok_or(TabError::NoActiveSession)?;
        let from = self.active_index().ok_or(TabError::NoActiveSession)?;
        let target = if delta < 0 {
            from.saturating_sub(1)
        } else {
            from.saturating_add(1)
                .min(self.tabs.len().saturating_sub(1))
        };
        self.reorder(id, target)
    }

    pub fn close_active(&mut self) -> Option<SessionId> {
        let index = self.active_index()?;
        let mut entry = self.tabs.remove(index);
        let id = entry.id;
        entry.runtime.fast_cancel();
        entry.session.begin_shutdown();
        self.closing.push(ClosingTab { entry });
        self.active_id = if self.tabs.is_empty() {
            None
        } else {
            Some(self.tabs[index.min(self.tabs.len() - 1)].id)
        };
        self.assert_invariants();
        Some(id)
    }

    #[must_use]
    pub fn drain_order(&mut self) -> Vec<SessionId> {
        if self.tabs.is_empty() {
            return Vec::new();
        }
        let start = self
            .drain_cursor
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
            .map_or(0, |index| (index + 1) % self.tabs.len());
        let order = (0..self.tabs.len())
            .map(|offset| self.tabs[(start + offset) % self.tabs.len()].id)
            .collect::<Vec<_>>();
        self.drain_cursor = order.last().copied();
        order
    }

    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut TabEntry> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    pub fn extract(&mut self, id: SessionId) -> Result<(TabEntry, usize), TabError> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == id)
            .ok_or(TabError::UnknownSession(id))?;
        let entry = self.tabs.remove(index);
        self.active_id = if self.tabs.is_empty() {
            None
        } else if self.active_id == Some(id) {
            Some(self.tabs[index.min(self.tabs.len() - 1)].id)
        } else {
            self.active_id
        };
        self.assert_invariants();
        Ok((entry, index))
    }

    pub fn insert(
        &mut self,
        entry: TabEntry,
        target: usize,
        activate: bool,
    ) -> Result<SessionId, TabError> {
        if !self.has_capacity() {
            return Err(TabError::LimitReached {
                limit: usize::from(self.max_count.get()),
            });
        }
        if self.tabs.iter().any(|tab| tab.id == entry.id)
            || self.closing.iter().any(|tab| tab.entry.id == entry.id)
        {
            return Err(TabError::DuplicateSession(entry.id));
        }
        let id = entry.id;
        let target = target.min(self.tabs.len());
        self.tabs.insert(target, entry);
        if activate || self.active_id.is_none() {
            self.active_id = Some(id);
        }
        self.assert_invariants();
        Ok(id)
    }

    pub fn mark_unread(&mut self, id: SessionId) -> bool {
        let Some(tab) = self.get_mut(id) else {
            return false;
        };
        let changed = !tab.unread;
        tab.unread = true;
        changed
    }

    pub fn record_background_bell(
        &mut self,
        id: SessionId,
        show_attention: bool,
    ) -> Result<BellGeneration, TabError> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == id)
            .ok_or(TabError::UnknownSession(id))?;
        if let Some(generation) = self.tabs[index].active_bell_generation {
            if show_attention {
                self.tabs[index].attention = true;
            }
            return Ok(generation);
        }
        let generation = BellGeneration(self.next_bell_generation);
        let next = self
            .next_bell_generation
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(TabError::BellGenerationExhausted)?;
        self.next_bell_generation = next;
        let tab = &mut self.tabs[index];
        tab.active_bell_generation = Some(generation);
        tab.attention = show_attention;
        Ok(generation)
    }

    pub fn acknowledge(&mut self, id: SessionId) -> Option<BellGeneration> {
        let tab = self.get_mut(id)?;
        tab.unread = false;
        tab.attention = false;
        tab.active_bell_generation.take()
    }

    pub fn toggle_bell_mute(
        &mut self,
        id: SessionId,
    ) -> Result<(bool, Option<BellGeneration>), TabError> {
        let tab = self.get_mut(id).ok_or(TabError::UnknownSession(id))?;
        tab.bell_muted = !tab.bell_muted;
        let invalidated = if tab.bell_muted {
            tab.attention = false;
            tab.active_bell_generation.take()
        } else {
            None
        };
        Ok((tab.bell_muted, invalidated))
    }

    pub fn apply_cwd_report(
        &mut self,
        id: SessionId,
        report: CwdReport,
        identity: &LocalIdentity,
    ) -> Option<Result<(), CwdRejectReason>> {
        let tab = self.get_mut(id)?;
        match validate_report(report, identity) {
            Ok(hint) => {
                tab.cwd_hint = Some(hint);
                tab.last_cwd_reject = None;
                Some(Ok(()))
            }
            Err(reason) => {
                tab.cwd_hint = None;
                tab.last_cwd_reject = Some(reason);
                Some(Err(reason))
            }
        }
    }

    fn active_index(&self) -> Option<usize> {
        let id = self.active_id?;
        self.tabs.iter().position(|tab| tab.id == id)
    }

    fn assert_invariants(&self) {
        debug_assert!(self.tabs.len() + self.closing.len() <= usize::from(self.max_count.get()));
        debug_assert_eq!(self.active_id.is_none(), self.tabs.is_empty());
        if let Some(id) = self.active_id {
            debug_assert_eq!(self.tabs.iter().filter(|tab| tab.id == id).count(), 1);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TabError {
    #[error("tab limit reached ({limit})")]
    LimitReached { limit: usize },
    #[error("session id space exhausted")]
    SessionIdExhausted,
    #[error("bell generation space exhausted")]
    BellGenerationExhausted,
    #[error("unknown session {0:?}")]
    UnknownSession(SessionId),
    #[error("duplicate session {0:?}")]
    DuplicateSession(SessionId),
    #[error("window has no active session")]
    NoActiveSession,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_include_last_value_then_report_exhaustion() {
        let mut allocator = SessionIdAllocator {
            next: NonZeroU64::new(u64::MAX),
        };
        assert_eq!(allocator.allocate().unwrap().get(), u64::MAX);
        assert!(matches!(
            allocator.allocate(),
            Err(TabError::SessionIdExhausted)
        ));
    }
    use std::{ffi::OsString, sync::Arc};

    fn entry() -> (TerminalSession, AppRuntime) {
        let runtime = crate::app::runtime::AppRuntimeBuilder::new(Arc::new(
            crate::app::runtime::CountingWake::default(),
        ))
        .build()
        .unwrap();
        let launch = crate::cli::LaunchRequest::Command(crate::cli::CommandSpec {
            program: OsString::from("/bin/true"),
            args: Vec::new(),
        });
        let session = TerminalSession::start(
            &launch,
            leyline_pty::SpawnDirectory::open(std::path::Path::new("/tmp")).unwrap(),
            &crate::config::EffectiveConfig::default(),
            crate::terminal::GridSize::new(8, 4).unwrap(),
            &runtime,
        )
        .unwrap();
        (session, runtime)
    }

    #[test]
    fn stable_ids_wrap_navigation_and_close_prefers_the_right_neighbor() {
        let (session, runtime) = entry();
        let mut manager = TabManager::bootstrap(session, runtime, NonZeroU8::new(3).unwrap());
        let (session, runtime) = entry();
        let second = manager.push(session, runtime, "two".into()).unwrap();
        let (session, runtime) = entry();
        let third = manager.push(session, runtime, "three".into()).unwrap();
        assert_eq!(manager.active_id(), Some(third));
        assert!(
            matches!(manager.activate_relative(1), Activation::Changed { to, .. } if to.get() == 1)
        );
        manager.activate(second).unwrap();
        manager.close_active();
        assert_eq!(manager.active_id(), Some(third));
        assert!(!manager.has_capacity());
    }

    #[test]
    fn ordinal_miss_is_a_consumed_noop_and_bar_hit_uses_half_open_rectangles() {
        let (session, runtime) = entry();
        let mut manager = TabManager::bootstrap(session, runtime, NonZeroU8::new(2).unwrap());
        assert_eq!(manager.activate_ordinal(9), Activation::Unchanged);
        let bar = TabBarPresentation::layout(
            &manager,
            200,
            120,
            &crate::config::TabsConfig {
                max_count: 2,
                bar_height: 32,
                min_width: 80,
                max_width: 240,
                show_close_button: true,
                new_tab_cwd: crate::config::NewTabCwdPolicy::Inherit,
                visibility: crate::config::TabBarVisibility::Always,
            },
            0,
        );
        assert_eq!(bar.hit([0, 0]).map(|hit| hit.0.get()), Some(1));
        assert_eq!(bar.hit([200, 0]), None);
        let close = bar.items[0].close_rect.expect("wide tab has close target");
        assert_eq!([close.width, close.height], [24, 24]);
        assert_eq!(
            bar.hit([close.x + close.width / 2, close.y + close.height / 2]),
            Some((manager.active_id().unwrap(), true))
        );
    }

    #[test]
    fn tabs_fill_the_bar_when_the_minimum_widths_fit() {
        let (session, runtime) = entry();
        let mut manager = TabManager::bootstrap(session, runtime, NonZeroU8::new(3).unwrap());
        let (session, runtime) = entry();
        manager.push(session, runtime, "two".into()).unwrap();
        let (session, runtime) = entry();
        manager.push(session, runtime, "three".into()).unwrap();

        let bar = TabBarPresentation::layout(
            &manager,
            1000,
            120,
            &crate::config::EffectiveConfig::default().tabs,
            0,
        );

        assert_eq!(
            bar.items[0].rect,
            PixelRect {
                x: 0,
                y: 0,
                width: 333,
                height: 32
            }
        );
        assert_eq!(bar.items[1].rect.x, 333);
        assert_eq!(
            bar.items[2].rect,
            PixelRect {
                x: 666,
                y: 0,
                width: 334,
                height: 32
            }
        );
        assert_eq!(bar.max_offset, 0);
    }

    #[test]
    fn drag_preview_moves_only_the_selected_label() {
        let (session, runtime) = entry();
        let mut manager = TabManager::bootstrap(session, runtime, NonZeroU8::new(2).unwrap());
        let first = manager.active_id().unwrap();
        let (session, runtime) = entry();
        manager.push(session, runtime, "two".into()).unwrap();
        let mut bar = TabBarPresentation::layout(
            &manager,
            800,
            120,
            &crate::config::EffectiveConfig::default().tabs,
            0,
        );

        bar.apply_drag_preview(first, 37);

        assert_eq!(bar.items[0].drag_offset_x, 37);
        assert_eq!(bar.items[1].drag_offset_x, 0);
    }

    #[test]
    fn cwd_reports_are_routed_by_stable_session_id_and_rejection_clears_hint() {
        let (session, runtime) = entry();
        let mut manager = TabManager::bootstrap(session, runtime, NonZeroU8::new(2).unwrap());
        let first = manager.active_id().unwrap();
        let (session, runtime) = entry();
        let second = manager.push(session, runtime, "two".into()).unwrap();
        let identity = LocalIdentity::new(Some("host".into()));

        assert_eq!(
            manager.apply_cwd_report(
                first,
                CwdReport::Set(b"file:///tmp/first".to_vec()),
                &identity
            ),
            Some(Ok(()))
        );
        assert_eq!(
            manager.apply_cwd_report(
                second,
                CwdReport::Set(b"file:///tmp/second".to_vec()),
                &identity
            ),
            Some(Ok(()))
        );
        assert_eq!(
            manager.tabs()[0].cwd_hint.as_ref().unwrap().path,
            std::path::Path::new("/tmp/first")
        );
        assert_eq!(
            manager.tabs()[1].cwd_hint.as_ref().unwrap().path,
            std::path::Path::new("/tmp/second")
        );

        assert_eq!(
            manager.apply_cwd_report(
                first,
                CwdReport::Set(b"file://remote/tmp/remote".to_vec()),
                &identity
            ),
            Some(Err(CwdRejectReason::RemoteAuthority))
        );
        assert!(manager.tabs()[0].cwd_hint.is_none());
        assert!(manager.tabs()[1].cwd_hint.is_some());
        assert_eq!(
            manager.apply_cwd_report(
                SessionId::from_raw(999),
                CwdReport::Set(b"file:///tmp/stale".to_vec()),
                &identity
            ),
            None
        );
    }

    #[test]
    fn attention_generation_is_reused_and_mute_preserves_unread() {
        let (session, runtime) = entry();
        let mut manager = TabManager::bootstrap(session, runtime, NonZeroU8::new(2).unwrap());
        let id = manager.active_id().unwrap();
        assert!(manager.mark_unread(id));
        let first = manager.record_background_bell(id, true).unwrap();
        let repeated = manager.record_background_bell(id, true).unwrap();
        assert_eq!(first, repeated);
        assert!(manager.tabs()[0].attention);
        let (muted, invalidated) = manager.toggle_bell_mute(id).unwrap();
        assert!(muted);
        assert_eq!(invalidated, Some(first));
        assert!(manager.tabs()[0].unread);
        assert!(!manager.tabs()[0].attention);
        let (muted, invalidated) = manager.toggle_bell_mute(id).unwrap();
        assert!(!muted);
        assert_eq!(invalidated, None);
        assert!(manager.tabs()[0].unread);
    }

    #[test]
    fn hidden_attention_episode_is_still_acknowledged() {
        let (session, runtime) = entry();
        let mut manager = TabManager::bootstrap(session, runtime, NonZeroU8::new(1).unwrap());
        let id = manager.active_id().unwrap();
        let generation = manager.record_background_bell(id, false).unwrap();
        assert!(!manager.tabs()[0].attention);
        assert_eq!(manager.acknowledge(id), Some(generation));
        assert_eq!(manager.acknowledge(id), None);
    }

    #[test]
    fn reorder_preserves_active_identity_and_fair_cursor() {
        let (session, runtime) = entry();
        let mut manager = TabManager::bootstrap(session, runtime, NonZeroU8::new(3).unwrap());
        let first = manager.active_id().unwrap();
        let (session, runtime) = entry();
        let second = manager.push(session, runtime, "two".into()).unwrap();
        let (session, runtime) = entry();
        let third = manager.push(session, runtime, "three".into()).unwrap();
        let _ = manager.drain_order();
        assert_eq!(
            manager.reorder(third, 0).unwrap(),
            ReorderOutcome::Changed { from: 2, to: 0 }
        );
        assert_eq!(manager.active_id(), Some(third));
        assert_eq!(
            manager.tabs().iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![third, first, second]
        );
        assert_eq!(manager.drain_order().len(), 3);
    }

    #[test]
    fn visibility_policy_removes_all_tab_hit_regions() {
        let (session, runtime) = entry();
        let manager = TabManager::bootstrap(session, runtime, NonZeroU8::new(2).unwrap());
        for visibility in [
            crate::config::TabBarVisibility::Multiple,
            crate::config::TabBarVisibility::Never,
        ] {
            let mut config = crate::config::EffectiveConfig::default().tabs;
            config.visibility = visibility;
            let bar = TabBarPresentation::layout(&manager, 200, 120, &config, 0);
            assert!(bar.bar.is_none());
            assert!(bar.items.is_empty());
        }
    }
}

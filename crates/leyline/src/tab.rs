use std::num::NonZeroU8;

use crate::{app::runtime::AppRuntime, session::TerminalSession};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionId(u64);

impl SessionId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

pub struct TabEntry {
    pub id: SessionId,
    pub session: TerminalSession,
    pub runtime: AppRuntime,
    pub title: String,
    pub unread: bool,
}

pub struct ClosingTab {
    pub entry: TabEntry,
}

pub struct TabManager {
    tabs: Vec<TabEntry>,
    closing: Vec<ClosingTab>,
    active_id: Option<SessionId>,
    next_id: u64,
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
pub struct TabBarItem {
    pub id: SessionId,
    pub title: String,
    pub rect: PixelRect,
    pub close_rect: Option<PixelRect>,
    pub active: bool,
    pub unread: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TabBarPresentation {
    pub bar: Option<PixelRect>,
    pub items: Vec<TabBarItem>,
    pub max_offset: u32,
    pub offset: u32,
}

impl TabBarPresentation {
    #[must_use]
    pub fn layout(
        manager: &TabManager,
        viewport_width: u32,
        scale_120: u32,
        config: &crate::config::TabsConfig,
        requested_offset: u32,
    ) -> Self {
        if manager.tabs.is_empty() {
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

#[allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]
impl TabManager {
    #[must_use]
    pub fn bootstrap(session: TerminalSession, runtime: AppRuntime, max_count: NonZeroU8) -> Self {
        let id = SessionId(1);
        Self {
            tabs: vec![TabEntry {
                id,
                session,
                runtime,
                title: "Shell".into(),
                unread: false,
            }],
            closing: Vec::new(),
            active_id: Some(id),
            next_id: 2,
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
        let id = SessionId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(TabError::SessionIdExhausted)?;
        self.tabs.push(TabEntry {
            id,
            session,
            runtime,
            title,
            unread: false,
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
        if let Some(tab) = self.active_mut() {
            tab.unread = false;
        }
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
    #[error("unknown session {0:?}")]
    UnknownSession(SessionId),
}

#[cfg(test)]
mod tests {
    use super::*;
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
}

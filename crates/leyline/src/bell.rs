use std::time::{Duration, Instant};

use crate::{config::BellConfig, tab::SessionId};

const VISUAL_BURST_CAP: Duration = Duration::from_millis(240);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct BellContext {
    pub session_id: SessionId,
    pub active: bool,
    pub window_focused: bool,
    pub muted: bool,
    pub session_effects_allowed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct BellEffects {
    pub record_attention_episode: bool,
    pub show_attention_marker: bool,
    pub schedule_visual: bool,
    pub enqueue_sound: bool,
    pub enqueue_notification: bool,
}

#[must_use]
pub fn decide(context: BellContext, config: &BellConfig) -> BellEffects {
    if !config.enabled || context.muted || !context.session_effects_allowed {
        return BellEffects::default();
    }
    if context.active && context.window_focused {
        return BellEffects {
            schedule_visual: config.visual,
            enqueue_sound: config.audible,
            ..BellEffects::default()
        };
    }
    BellEffects {
        record_attention_episode: true,
        show_attention_marker: config.attention,
        enqueue_sound: config.audible && (context.window_focused || config.audible_when_unfocused),
        enqueue_notification: config.desktop_notifications,
        schedule_visual: false,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct VisualBellState {
    owner: Option<SessionId>,
    burst_started_at: Option<Instant>,
    visible_until: Option<Instant>,
}

impl VisualBellState {
    pub fn schedule(&mut self, owner: SessionId, now: Instant, duration: Duration) {
        let same_burst = self.owner == Some(owner)
            && self.visible_until.is_some_and(|deadline| deadline > now)
            && self.burst_started_at.is_some();
        let started = if same_burst {
            self.burst_started_at.unwrap_or(now)
        } else {
            now
        };
        let cap = duration.max(VISUAL_BURST_CAP);
        let requested = now.checked_add(duration);
        let maximum = started.checked_add(cap);
        self.owner = Some(owner);
        self.burst_started_at = Some(started);
        self.visible_until = match (requested, maximum) {
            (Some(requested), Some(maximum)) => Some(requested.min(maximum)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => Some(now),
        };
    }

    #[must_use]
    pub fn active_for(&self, owner: SessionId, now: Instant) -> bool {
        self.owner == Some(owner) && self.visible_until.is_some_and(|deadline| deadline > now)
    }

    pub fn expire(&mut self, now: Instant) -> bool {
        if self.visible_until.is_none_or(|deadline| deadline > now) {
            return false;
        }
        self.cancel();
        true
    }

    pub fn cancel(&mut self) {
        *self = Self::default();
    }

    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.visible_until
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VisualBellPresentation {
    pub active: bool,
    pub color: u32,
    pub intensity: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_matrix_keeps_background_attention_separate() {
        let config = BellConfig::default();
        let id = SessionId::from_raw(1);
        let focused = decide(
            BellContext {
                session_id: id,
                active: true,
                window_focused: true,
                muted: false,
                session_effects_allowed: true,
            },
            &config,
        );
        assert!(focused.schedule_visual);
        assert!(!focused.record_attention_episode);
        let background = decide(
            BellContext {
                session_id: id,
                active: false,
                window_focused: true,
                muted: false,
                session_effects_allowed: true,
            },
            &config,
        );
        assert!(background.record_attention_episode);
        assert!(background.show_attention_marker);
        assert!(!background.schedule_visual);
    }

    #[test]
    fn repeated_visual_bells_have_a_burst_cap() {
        let id = SessionId::from_raw(1);
        let start = Instant::now();
        let mut state = VisualBellState::default();
        state.schedule(id, start, Duration::from_millis(120));
        state.schedule(
            id,
            start + Duration::from_millis(110),
            Duration::from_millis(120),
        );
        state.schedule(
            id,
            start + Duration::from_millis(220),
            Duration::from_millis(120),
        );
        assert_eq!(
            state.deadline(),
            start.checked_add(Duration::from_millis(240))
        );
    }
}

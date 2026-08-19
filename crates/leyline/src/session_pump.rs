//! Bounded session-drain policy and explicit effects.

use std::time::Duration;

pub const WINDOW_EVENT_BUDGET: usize = 64;
pub const WINDOW_BYTE_BUDGET: usize = 1024 * 1024;
pub const WINDOW_TIME_BUDGET: Duration = Duration::from_millis(2);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionPumpEffect {
    pub needs_frame: bool,
    pub title_changed: bool,
    pub bell: bool,
    pub completed: Vec<crate::tab::SessionId>,
}

pub struct SessionPump;

impl SessionPump {
    #[must_use]
    pub fn limits() -> (usize, usize, Duration) {
        (WINDOW_EVENT_BUDGET, WINDOW_BYTE_BUDGET, WINDOW_TIME_BUDGET)
    }
}

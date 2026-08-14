//! Central product security limits and pure policy result types.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub const MAX_TITLE_BYTES: usize = 1024;
pub const MAX_URI_BYTES: usize = 4096;
pub const MAX_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PTY_REPLY_BYTES: usize = 64 * 1024;
pub const MAX_HYPERLINKS: usize = 4096;
pub const MAX_HYPERLINK_BYTES: usize = 4096;
pub const MAX_ZERO_WIDTH_PER_CELL: usize = 64;
pub const MAX_ZERO_WIDTH_TOTAL: usize = 64 * 1024;
pub const INTERACTIVE_INPUT_RESERVE: usize = 64 * 1024;
const AUDIT_LOG_WINDOW: Duration = Duration::from_mins(1);
const AUDIT_LOG_BURST: u8 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditLogDecision {
    Emit { previously_suppressed: u64 },
    Suppress,
}

#[derive(Debug, Default)]
pub struct MetadataRateLimiter {
    window_started: Option<Instant>,
    emitted: u8,
    suppressed: u64,
}

impl MetadataRateLimiter {
    #[must_use]
    pub fn record(&mut self, now: Instant) -> AuditLogDecision {
        let new_window = self
            .window_started
            .is_none_or(|started| now.saturating_duration_since(started) >= AUDIT_LOG_WINDOW);
        if new_window {
            let previously_suppressed = std::mem::take(&mut self.suppressed);
            self.window_started = Some(now);
            self.emitted = 1;
            return AuditLogDecision::Emit {
                previously_suppressed,
            };
        }
        if self.emitted < AUDIT_LOG_BURST {
            self.emitted += 1;
            AuditLogDecision::Emit {
                previously_suppressed: 0,
            }
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            AuditLogDecision::Suppress
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyDecision<T> {
    Allow(T),
    Confirm { value: T, reason: RiskReason },
    Reject { reason: RejectReason },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RiskReason {
    MultilinePaste,
    ControlCharacters,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    Empty,
    TooLarge { limit: usize, observed: usize },
    Nul,
    InvalidUtf8,
    Unsupported,
    DisallowedScheme,
    StaleGeneration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultClass {
    RejectedInput,
    CapacityPressure,
    TransientWsi,
    SessionFailure,
    FatalPlatform,
    FatalDevice,
    InternalInvariant,
}

#[must_use]
pub fn validate_title(value: &str) -> PolicyDecision<Arc<str>> {
    if value.contains('\0') {
        return PolicyDecision::Reject {
            reason: RejectReason::Nul,
        };
    }
    if value.len() > MAX_TITLE_BYTES {
        return PolicyDecision::Reject {
            reason: RejectReason::TooLarge {
                limit: MAX_TITLE_BYTES,
                observed: value.len(),
            },
        };
    }
    PolicyDecision::Allow(Arc::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_limit_is_measured_in_utf8_bytes_and_nul_is_rejected() {
        assert_eq!(MAX_TITLE_BYTES, leyline_gfx::MAX_WINDOW_TITLE_BYTES);
        assert!(matches!(validate_title("safe"), PolicyDecision::Allow(_)));
        assert!(matches!(
            validate_title(&"中".repeat(342)),
            PolicyDecision::Reject {
                reason: RejectReason::TooLarge { .. }
            }
        ));
        assert_eq!(
            validate_title("bad\0title"),
            PolicyDecision::Reject {
                reason: RejectReason::Nul
            }
        );
    }

    #[test]
    fn metadata_limiter_emits_five_per_minute_and_reports_suppression() {
        let start = Instant::now();
        let mut limiter = MetadataRateLimiter::default();
        for _ in 0..AUDIT_LOG_BURST {
            assert_eq!(
                limiter.record(start),
                AuditLogDecision::Emit {
                    previously_suppressed: 0
                }
            );
        }
        assert_eq!(limiter.record(start), AuditLogDecision::Suppress);
        assert_eq!(limiter.record(start), AuditLogDecision::Suppress);
        assert_eq!(
            limiter.record(start + AUDIT_LOG_WINDOW),
            AuditLogDecision::Emit {
                previously_suppressed: 2
            }
        );
    }
}

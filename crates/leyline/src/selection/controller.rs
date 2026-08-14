use std::{collections::HashMap, sync::Arc};

use leyline_gfx::{KeyInput, KeyState, LogicalKey};

use crate::clipboard::{
    PasteConfirmationOverlay, PastePolicy, TransferError, TransferTarget, evaluate_paste,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequestToken(u64);

impl RequestToken {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceToken(u64);

impl SourceToken {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionMode {
    Normal,
    ConfirmPaste { request: RequestToken },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestStart {
    pub request: RequestToken,
    pub cancel: Option<RequestToken>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PasteTransition {
    IgnoreStale,
    Failed(TransferError),
    Rejected,
    Paste(String),
    Confirming,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfirmationOutcome {
    NotActive,
    Consumed,
    Closed,
    Paste(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PasteStateKind {
    Idle,
    Reading,
    Confirming,
}

#[derive(Clone, Debug)]
enum PasteState {
    Idle,
    Reading {
        request: RequestToken,
        target: TransferTarget,
    },
    Confirming {
        request: RequestToken,
        text: String,
        overlay: PasteConfirmationOverlay,
        ime_suspended: bool,
    },
}

pub struct SelectionController {
    sources: HashMap<SourceToken, Arc<[u8]>>,
    next_source: u64,
    next_request: u64,
    overlay_revision: u64,
    paste: PasteState,
    shutdown: bool,
}

impl Default for SelectionController {
    fn default() -> Self {
        Self {
            sources: HashMap::new(),
            next_source: 1,
            next_request: 1,
            overlay_revision: 0,
            paste: PasteState::Idle,
            shutdown: false,
        }
    }
}

impl SelectionController {
    #[must_use]
    pub fn publish(&mut self, text: String) -> Option<SourceToken> {
        if self.shutdown || text.is_empty() || text.len() > crate::security::MAX_CLIPBOARD_BYTES {
            return None;
        }
        let token = SourceToken(self.next_source);
        self.next_source = self.next_source.wrapping_add(1).max(1);
        self.sources.insert(token, Arc::from(text.into_bytes()));
        Some(token)
    }

    #[must_use]
    pub fn source_bytes(&self, source: SourceToken) -> Option<Arc<[u8]>> {
        self.sources.get(&source).cloned()
    }

    pub fn source_cancelled(&mut self, source: SourceToken) {
        self.sources.remove(&source);
    }

    #[must_use]
    pub fn begin_request(&mut self, target: TransferTarget) -> Option<RequestStart> {
        if self.shutdown {
            return None;
        }
        let cancel = self.active_request();
        let request = RequestToken(self.next_request);
        self.next_request = self.next_request.wrapping_add(1).max(1);
        self.paste = PasteState::Reading { request, target };
        Some(RequestStart { request, cancel })
    }

    pub fn request_failed(&mut self, request: RequestToken) {
        if self.active_request() == Some(request) {
            self.paste = PasteState::Idle;
        }
    }

    pub fn transfer_completed(
        &mut self,
        request: RequestToken,
        target: TransferTarget,
        result: Result<String, TransferError>,
        confirm_multiline: bool,
    ) -> PasteTransition {
        if !matches!(
            self.paste,
            PasteState::Reading {
                request: current,
                target: current_target,
            } if current == request && current_target == target
        ) {
            return PasteTransition::IgnoreStale;
        }
        let text = match result {
            Ok(text) => text,
            Err(error) => {
                self.paste = PasteState::Idle;
                return PasteTransition::Failed(error);
            }
        };
        match evaluate_paste(&text, confirm_multiline) {
            PastePolicy::Allowed(text) => {
                self.paste = PasteState::Idle;
                PasteTransition::Paste(text)
            }
            PastePolicy::Rejected => {
                self.paste = PasteState::Idle;
                PasteTransition::Rejected
            }
            PastePolicy::NeedsConfirmation {
                text,
                bytes,
                lines,
                risk,
            } => {
                self.overlay_revision = self.overlay_revision.wrapping_add(1).max(1);
                self.paste = PasteState::Confirming {
                    request,
                    text,
                    overlay: PasteConfirmationOverlay {
                        revision: self.overlay_revision,
                        source: target,
                        bytes,
                        lines,
                        risk,
                    },
                    ime_suspended: false,
                };
                PasteTransition::Confirming
            }
        }
    }

    #[must_use]
    pub fn confirmation_input(&mut self, key: &KeyInput) -> ConfirmationOutcome {
        if !matches!(self.paste, PasteState::Confirming { .. }) {
            return ConfirmationOutcome::NotActive;
        }
        if key.state == KeyState::Released {
            return ConfirmationOutcome::Consumed;
        }
        match key.logical_key {
            LogicalKey::Enter | LogicalKey::Character('y' | 'Y') => {
                let PasteState::Confirming { text, .. } =
                    std::mem::replace(&mut self.paste, PasteState::Idle)
                else {
                    return ConfirmationOutcome::NotActive;
                };
                ConfirmationOutcome::Paste(text)
            }
            _ => {
                self.paste = PasteState::Idle;
                ConfirmationOutcome::Closed
            }
        }
    }

    #[must_use]
    pub fn cancel_paste(&mut self) -> Option<RequestToken> {
        let request = self.active_request();
        self.paste = PasteState::Idle;
        request
    }

    pub fn shutdown(&mut self) -> Option<RequestToken> {
        self.shutdown = true;
        let request = self.cancel_paste();
        self.sources.clear();
        request
    }

    #[must_use]
    pub fn interaction_mode(&self) -> InteractionMode {
        match self.paste {
            PasteState::Confirming { request, .. } => InteractionMode::ConfirmPaste { request },
            PasteState::Idle | PasteState::Reading { .. } => InteractionMode::Normal,
        }
    }

    #[must_use]
    pub fn overlay(&self) -> Option<&PasteConfirmationOverlay> {
        match &self.paste {
            PasteState::Confirming { overlay, .. } => Some(overlay),
            PasteState::Idle | PasteState::Reading { .. } => None,
        }
    }

    pub fn set_modal_ime_suspended(&mut self, suspended: bool) {
        if let PasteState::Confirming { ime_suspended, .. } = &mut self.paste {
            *ime_suspended = suspended;
        }
    }

    #[must_use]
    pub fn modal_ime_suspended(&self) -> bool {
        matches!(
            self.paste,
            PasteState::Confirming {
                ime_suspended: true,
                ..
            }
        )
    }

    #[must_use]
    pub fn debug_snapshot(&self) -> SelectionDebugSnapshot {
        SelectionDebugSnapshot {
            paste: match self.paste {
                PasteState::Idle => PasteStateKind::Idle,
                PasteState::Reading { .. } => PasteStateKind::Reading,
                PasteState::Confirming { .. } => PasteStateKind::Confirming,
            },
            request: self.active_request(),
            source_count: self.sources.len(),
            shutdown: self.shutdown,
        }
    }

    fn active_request(&self) -> Option<RequestToken> {
        match self.paste {
            PasteState::Reading { request, .. } | PasteState::Confirming { request, .. } => {
                Some(request)
            }
            PasteState::Idle => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionDebugSnapshot {
    paste: PasteStateKind,
    pub request: Option<RequestToken>,
    pub source_count: usize,
    pub shutdown: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_and_stale_completion_cannot_paste_old_content() {
        let mut controller = SelectionController::default();
        let first = controller
            .begin_request(TransferTarget::Primary)
            .expect("request");
        let second = controller
            .begin_request(TransferTarget::Primary)
            .expect("request");
        assert_eq!(second.cancel, Some(first.request));
        assert_eq!(
            controller.transfer_completed(
                first.request,
                TransferTarget::Primary,
                Ok("stale".into()),
                true,
            ),
            PasteTransition::IgnoreStale
        );
        assert_eq!(
            controller.transfer_completed(
                second.request,
                TransferTarget::Primary,
                Ok("fresh".into()),
                true,
            ),
            PasteTransition::Paste("fresh".into())
        );
    }

    #[test]
    fn every_confirmation_press_exits_or_handles_the_modal() {
        let mut controller = SelectionController::default();
        let request = controller
            .begin_request(TransferTarget::Primary)
            .expect("request")
            .request;
        assert_eq!(
            controller.transfer_completed(
                request,
                TransferTarget::Primary,
                Ok("one\ntwo".into()),
                true,
            ),
            PasteTransition::Confirming
        );
        let key = KeyInput {
            serial: leyline_gfx::InputSerial {
                seat: leyline_gfx::SeatToken::new(0, 1),
                value: 1,
                kind: leyline_gfx::SerialKind::Keyboard,
            },
            time_ms: 1,
            physical_keycode: 1,
            utf8: Some("x".into()),
            modifiers: leyline_gfx::ModifiersState::default(),
            shortcut_modifiers: leyline_gfx::ModifierMask::empty(),
            logical_key: LogicalKey::Character('x'),
            state: KeyState::Pressed,
            repeat: false,
        };
        assert_eq!(
            controller.confirmation_input(&key),
            ConfirmationOutcome::Closed
        );
        assert_eq!(controller.interaction_mode(), InteractionMode::Normal);
    }

    #[test]
    fn transfer_and_queue_failures_leave_the_controller_idle() {
        let mut controller = SelectionController::default();
        let request = controller
            .begin_request(TransferTarget::Primary)
            .expect("request")
            .request;
        assert_eq!(
            controller.transfer_completed(
                request,
                TransferTarget::Primary,
                Err(TransferError::Timeout),
                true,
            ),
            PasteTransition::Failed(TransferError::Timeout)
        );
        assert_eq!(controller.interaction_mode(), InteractionMode::Normal);

        let request = controller
            .begin_request(TransferTarget::Primary)
            .expect("request")
            .request;
        controller.request_failed(request);
        assert_eq!(controller.debug_snapshot().request, None);
    }
}

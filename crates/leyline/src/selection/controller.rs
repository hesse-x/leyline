use std::{collections::HashMap, sync::Arc};

use leyline_gfx::{KeyInput, KeyState, LogicalKey};

use crate::{
    clipboard::{
        PasteConfirmationOverlay, PastePolicy, TransferError, TransferTarget, evaluate_paste,
    },
    tab::SessionId,
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
pub enum OfferState {
    Unavailable,
    Empty,
    Unsupported,
    TextAvailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionMode {
    Normal,
    ConfirmPaste { request: RequestToken },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PasteCancellation {
    pub request: Option<RequestToken>,
    pub resume_ime: bool,
    pub overlay_changed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublishStart {
    pub source: SourceToken,
    pub replaced: Option<SourceToken>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestStart {
    pub request: RequestToken,
    pub cancel: PasteCancellation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PasteTransition {
    IgnoreStale,
    Failed(TransferError),
    Rejected,
    Noop,
    Paste { owner: SessionId, text: String },
    Confirming,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfirmationOutcome {
    NotActive,
    Consumed,
    Closed,
    Paste { owner: SessionId, text: String },
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
        owner: SessionId,
        offer_generation: u64,
    },
    Confirming {
        request: RequestToken,
        target: TransferTarget,
        owner: SessionId,
        offer_generation: u64,
        text: String,
        overlay: PasteConfirmationOverlay,
        ime_suspended: bool,
    },
}

#[derive(Clone, Copy, Debug)]
struct TargetState {
    offer_generation: u64,
    offer: OfferState,
    current_source: Option<SourceToken>,
}

impl Default for TargetState {
    fn default() -> Self {
        Self {
            offer_generation: 0,
            offer: OfferState::Unavailable,
            current_source: None,
        }
    }
}

#[derive(Clone, Debug)]
struct SourceRecord {
    target: TransferTarget,
    bytes: Arc<[u8]>,
}

pub struct SelectionController {
    clipboard: TargetState,
    primary: TargetState,
    sources: HashMap<SourceToken, SourceRecord>,
    next_source: u64,
    next_request: u64,
    overlay_revision: u64,
    paste: PasteState,
    shutdown: bool,
}

impl Default for SelectionController {
    fn default() -> Self {
        Self {
            clipboard: TargetState::default(),
            primary: TargetState::default(),
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
    pub fn prepare_publish(
        &mut self,
        target: TransferTarget,
        text: String,
    ) -> Option<PublishStart> {
        if self.shutdown || text.is_empty() || text.len() > crate::security::MAX_CLIPBOARD_BYTES {
            return None;
        }
        let source = SourceToken(self.next_source);
        self.next_source = self.next_source.wrapping_add(1).max(1);
        let replaced = self.target(target).current_source;
        self.sources.insert(
            source,
            SourceRecord {
                target,
                bytes: Arc::from(text.into_bytes()),
            },
        );
        Some(PublishStart { source, replaced })
    }

    pub fn publish_submitted(&mut self, target: TransferTarget, source: SourceToken) {
        if self
            .sources
            .get(&source)
            .is_none_or(|record| record.target != target)
        {
            return;
        }
        if let Some(replaced) = self.target(target).current_source
            && replaced != source
        {
            self.sources.remove(&replaced);
        }
        self.target_mut(target).current_source = Some(source);
    }

    pub fn publish_failed(&mut self, source: SourceToken) {
        let Some(record) = self.sources.remove(&source) else {
            return;
        };
        if self.target(record.target).current_source == Some(source) {
            self.target_mut(record.target).current_source = None;
        }
    }

    #[must_use]
    pub fn source_bytes(&self, target: TransferTarget, source: SourceToken) -> Option<Arc<[u8]>> {
        self.sources
            .get(&source)
            .filter(|record| record.target == target)
            .map(|record| record.bytes.clone())
    }

    pub fn source_cancelled(&mut self, target: TransferTarget, source: SourceToken) {
        if self
            .sources
            .get(&source)
            .is_none_or(|record| record.target != target)
        {
            return;
        }
        self.sources.remove(&source);
        if self.target(target).current_source == Some(source) {
            self.target_mut(target).current_source = None;
        }
    }

    #[must_use]
    pub fn offer_changed(
        &mut self,
        target: TransferTarget,
        offer: OfferState,
    ) -> PasteCancellation {
        let state = self.target_mut(target);
        state.offer_generation = state.offer_generation.wrapping_add(1).max(1);
        state.offer = offer;
        if offer == OfferState::Unavailable
            && let Some(source) = self.target_mut(target).current_source.take()
        {
            self.sources.remove(&source);
        }
        if self.paste_target() == Some(target) {
            self.cancel_paste()
        } else {
            PasteCancellation::default()
        }
    }

    #[must_use]
    pub fn begin_request(
        &mut self,
        target: TransferTarget,
        owner: SessionId,
    ) -> Option<RequestStart> {
        if self.shutdown {
            return None;
        }
        let cancel = self.cancel_paste();
        let request = RequestToken(self.next_request);
        self.next_request = self.next_request.wrapping_add(1).max(1);
        self.paste = PasteState::Reading {
            request,
            target,
            owner,
            offer_generation: self.target(target).offer_generation,
        };
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
        active_owner: SessionId,
    ) -> PasteTransition {
        let PasteState::Reading {
            request: current,
            target: current_target,
            owner,
            offer_generation,
        } = self.paste
        else {
            return PasteTransition::IgnoreStale;
        };
        if current != request || current_target != target {
            return PasteTransition::IgnoreStale;
        }
        if owner != active_owner || offer_generation != self.target(target).offer_generation {
            self.paste = PasteState::Idle;
            return PasteTransition::IgnoreStale;
        }
        let text = match result {
            Ok(text) if text.is_empty() => {
                self.paste = PasteState::Idle;
                return PasteTransition::Noop;
            }
            Ok(text) => text,
            Err(error) => {
                self.paste = PasteState::Idle;
                return PasteTransition::Failed(error);
            }
        };
        match evaluate_paste(&text, confirm_multiline) {
            PastePolicy::Allowed(text) => {
                self.paste = PasteState::Idle;
                PasteTransition::Paste { owner, text }
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
                    target,
                    owner,
                    offer_generation,
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
    pub fn confirmation_input(
        &mut self,
        key: &KeyInput,
        active_owner: SessionId,
    ) -> ConfirmationOutcome {
        let PasteState::Confirming {
            owner,
            target,
            offer_generation,
            ..
        } = self.paste
        else {
            return ConfirmationOutcome::NotActive;
        };
        if owner != active_owner || offer_generation != self.target(target).offer_generation {
            self.paste = PasteState::Idle;
            return ConfirmationOutcome::Closed;
        }
        if key.state == KeyState::Released {
            return ConfirmationOutcome::Consumed;
        }
        match key.logical_key {
            LogicalKey::Enter | LogicalKey::Character('y' | 'Y') => {
                let PasteState::Confirming { owner, text, .. } =
                    std::mem::replace(&mut self.paste, PasteState::Idle)
                else {
                    unreachable!()
                };
                ConfirmationOutcome::Paste { owner, text }
            }
            _ => {
                self.paste = PasteState::Idle;
                ConfirmationOutcome::Closed
            }
        }
    }

    #[must_use]
    pub fn cancel_paste(&mut self) -> PasteCancellation {
        let request = match self.paste {
            PasteState::Reading { request, .. } => Some(request),
            PasteState::Idle | PasteState::Confirming { .. } => None,
        };
        let resume_ime = self.modal_ime_suspended();
        let overlay_changed = matches!(self.paste, PasteState::Confirming { .. });
        self.paste = PasteState::Idle;
        PasteCancellation {
            request,
            resume_ime,
            overlay_changed,
        }
    }

    pub fn shutdown(&mut self) -> PasteCancellation {
        self.shutdown = true;
        let cancellation = self.cancel_paste();
        self.sources.clear();
        self.clipboard.current_source = None;
        self.primary.current_source = None;
        cancellation
    }

    #[must_use]
    pub fn interaction_mode(&self) -> InteractionMode {
        match self.paste {
            PasteState::Confirming { request, .. } => InteractionMode::ConfirmPaste { request },
            _ => InteractionMode::Normal,
        }
    }
    #[must_use]
    pub fn overlay(&self) -> Option<&PasteConfirmationOverlay> {
        match &self.paste {
            PasteState::Confirming { overlay, .. } => Some(overlay),
            _ => None,
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
            clipboard_offer: self.clipboard.offer,
            clipboard_generation: self.clipboard.offer_generation,
            clipboard_source: self.clipboard.current_source,
            primary_offer: self.primary.offer,
            primary_generation: self.primary.offer_generation,
            primary_source: self.primary.current_source,
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
    fn paste_target(&self) -> Option<TransferTarget> {
        match self.paste {
            PasteState::Reading { target, .. } | PasteState::Confirming { target, .. } => {
                Some(target)
            }
            PasteState::Idle => None,
        }
    }
    fn target(&self, target: TransferTarget) -> &TargetState {
        match target {
            TransferTarget::Clipboard => &self.clipboard,
            TransferTarget::Primary => &self.primary,
        }
    }
    fn target_mut(&mut self, target: TransferTarget) -> &mut TargetState {
        match target {
            TransferTarget::Clipboard => &mut self.clipboard,
            TransferTarget::Primary => &mut self.primary,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionDebugSnapshot {
    paste: PasteStateKind,
    pub request: Option<RequestToken>,
    pub source_count: usize,
    pub shutdown: bool,
    pub clipboard_offer: OfferState,
    pub clipboard_generation: u64,
    pub clipboard_source: Option<SourceToken>,
    pub primary_offer: OfferState,
    pub primary_generation: u64,
    pub primary_source: Option<SourceToken>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(value: u64) -> SessionId {
        SessionId::from_raw(value)
    }

    #[test]
    fn sources_are_independent_and_stale_cancellation_is_safe() {
        let mut controller = SelectionController::default();
        let clipboard = controller
            .prepare_publish(TransferTarget::Clipboard, "clip".into())
            .unwrap();
        controller.publish_submitted(TransferTarget::Clipboard, clipboard.source);
        let primary = controller
            .prepare_publish(TransferTarget::Primary, "primary".into())
            .unwrap();
        controller.publish_submitted(TransferTarget::Primary, primary.source);
        let replacement = controller
            .prepare_publish(TransferTarget::Clipboard, "new".into())
            .unwrap();
        assert_eq!(replacement.replaced, Some(clipboard.source));
        controller.publish_submitted(TransferTarget::Clipboard, replacement.source);
        controller.source_cancelled(TransferTarget::Clipboard, clipboard.source);
        let snapshot = controller.debug_snapshot();
        assert_eq!(snapshot.clipboard_source, Some(replacement.source));
        assert_eq!(snapshot.primary_source, Some(primary.source));
    }

    #[test]
    fn offer_change_only_cancels_matching_target() {
        let mut controller = SelectionController::default();
        let _ = controller.offer_changed(TransferTarget::Clipboard, OfferState::TextAvailable);
        let request = controller
            .begin_request(TransferTarget::Clipboard, owner(1))
            .unwrap();
        assert_eq!(
            controller
                .offer_changed(TransferTarget::Primary, OfferState::Empty)
                .request,
            None
        );
        assert_eq!(controller.debug_snapshot().request, Some(request.request));
        assert_eq!(
            controller
                .offer_changed(TransferTarget::Clipboard, OfferState::Empty)
                .request,
            Some(request.request)
        );
    }

    #[test]
    fn unrelated_offer_change_preserves_confirmation_and_ime_state() {
        let mut controller = SelectionController::default();
        let _ = controller.offer_changed(TransferTarget::Clipboard, OfferState::TextAvailable);
        let request = controller
            .begin_request(TransferTarget::Clipboard, owner(1))
            .unwrap()
            .request;
        assert_eq!(
            controller.transfer_completed(
                request,
                TransferTarget::Clipboard,
                Ok("one\ntwo".into()),
                true,
                owner(1)
            ),
            PasteTransition::Confirming
        );
        controller.set_modal_ime_suspended(true);
        assert_eq!(
            controller.offer_changed(TransferTarget::Primary, OfferState::Empty),
            PasteCancellation::default()
        );
        assert!(controller.modal_ime_suspended());
        assert!(matches!(
            controller.interaction_mode(),
            InteractionMode::ConfirmPaste { .. }
        ));
        let cancellation = controller.offer_changed(TransferTarget::Clipboard, OfferState::Empty);
        assert!(cancellation.resume_ime);
        assert!(cancellation.overlay_changed);
        assert_eq!(cancellation.request, None);
    }

    #[test]
    fn unavailable_clears_only_its_current_source() {
        let mut controller = SelectionController::default();
        let clipboard = controller
            .prepare_publish(TransferTarget::Clipboard, "clip".into())
            .unwrap();
        controller.publish_submitted(TransferTarget::Clipboard, clipboard.source);
        let primary = controller
            .prepare_publish(TransferTarget::Primary, "primary".into())
            .unwrap();
        controller.publish_submitted(TransferTarget::Primary, primary.source);
        let _ = controller.offer_changed(TransferTarget::Clipboard, OfferState::Unavailable);
        assert_eq!(controller.debug_snapshot().clipboard_source, None);
        assert_eq!(
            controller.debug_snapshot().primary_source,
            Some(primary.source)
        );
        assert!(
            controller
                .source_bytes(TransferTarget::Clipboard, clipboard.source)
                .is_none()
        );
    }

    #[test]
    fn empty_transfer_is_a_noop() {
        let mut controller = SelectionController::default();
        let _ = controller.offer_changed(TransferTarget::Clipboard, OfferState::TextAvailable);
        let request = controller
            .begin_request(TransferTarget::Clipboard, owner(1))
            .unwrap()
            .request;
        assert_eq!(
            controller.transfer_completed(
                request,
                TransferTarget::Clipboard,
                Ok(String::new()),
                true,
                owner(1)
            ),
            PasteTransition::Noop
        );
        assert_eq!(controller.debug_snapshot().request, None);
    }

    #[test]
    fn replacement_owner_and_generation_make_completions_stale() {
        let mut controller = SelectionController::default();
        let _ = controller.offer_changed(TransferTarget::Primary, OfferState::TextAvailable);
        let first = controller
            .begin_request(TransferTarget::Primary, owner(1))
            .unwrap();
        let second = controller
            .begin_request(TransferTarget::Primary, owner(1))
            .unwrap();
        assert_eq!(second.cancel.request, Some(first.request));
        assert_eq!(
            controller.transfer_completed(
                first.request,
                TransferTarget::Primary,
                Ok("stale".into()),
                true,
                owner(1)
            ),
            PasteTransition::IgnoreStale
        );
        assert_eq!(
            controller.transfer_completed(
                second.request,
                TransferTarget::Primary,
                Ok("fresh".into()),
                true,
                owner(2)
            ),
            PasteTransition::IgnoreStale
        );
        let _ = controller.offer_changed(TransferTarget::Primary, OfferState::TextAvailable);
        assert_eq!(
            controller.transfer_completed(
                second.request,
                TransferTarget::Primary,
                Ok("old offer".into()),
                true,
                owner(1)
            ),
            PasteTransition::IgnoreStale
        );
    }

    #[test]
    fn risky_paste_is_bound_to_owner() {
        let mut controller = SelectionController::default();
        let _ = controller.offer_changed(TransferTarget::Primary, OfferState::TextAvailable);
        let request = controller
            .begin_request(TransferTarget::Primary, owner(1))
            .unwrap()
            .request;
        assert_eq!(
            controller.transfer_completed(
                request,
                TransferTarget::Primary,
                Ok("one\ntwo".into()),
                true,
                owner(1)
            ),
            PasteTransition::Confirming
        );
        let mut key = KeyInput {
            serial: leyline_gfx::InputSerial {
                seat: leyline_gfx::SeatToken::new(0, 1),
                value: 1,
                kind: leyline_gfx::SerialKind::Keyboard,
            },
            time_ms: 1,
            physical_keycode: 1,
            shortcut_digit_row: None,
            utf8: None,
            modifiers: leyline_gfx::ModifiersState::default(),
            shortcut_modifiers: leyline_gfx::ModifierMask::empty(),
            logical_key: LogicalKey::Enter,
            state: KeyState::Pressed,
            repeat: false,
        };
        assert_eq!(
            controller.confirmation_input(&key, owner(2)),
            ConfirmationOutcome::Closed
        );

        let request = controller
            .begin_request(TransferTarget::Primary, owner(1))
            .unwrap()
            .request;
        assert_eq!(
            controller.transfer_completed(
                request,
                TransferTarget::Primary,
                Ok("one\ntwo".into()),
                true,
                owner(1)
            ),
            PasteTransition::Confirming
        );
        key.state = KeyState::Released;
        assert_eq!(
            controller.confirmation_input(&key, owner(1)),
            ConfirmationOutcome::Consumed
        );
        key.state = KeyState::Pressed;
        assert_eq!(
            controller.confirmation_input(&key, owner(1)),
            ConfirmationOutcome::Paste {
                owner: owner(1),
                text: "one\ntwo".into()
            }
        );
    }

    #[test]
    fn transfer_and_queue_failures_leave_the_controller_idle() {
        let mut controller = SelectionController::default();
        let _ = controller.offer_changed(TransferTarget::Primary, OfferState::TextAvailable);
        let request = controller
            .begin_request(TransferTarget::Primary, owner(1))
            .unwrap()
            .request;
        assert_eq!(
            controller.transfer_completed(
                request,
                TransferTarget::Primary,
                Err(TransferError::Timeout),
                true,
                owner(1)
            ),
            PasteTransition::Failed(TransferError::Timeout)
        );
        assert_eq!(controller.interaction_mode(), InteractionMode::Normal);

        let request = controller
            .begin_request(TransferTarget::Primary, owner(1))
            .unwrap()
            .request;
        controller.request_failed(request);
        assert_eq!(controller.debug_snapshot().request, None);
    }
}

use super::{KeyboardProtocolState, KittyKeyboardFlags, ModifyOtherKeysLevel};

const MAX_SEQUENCE_BYTES: usize = 64;
const MAX_KEYBOARD_STACK_DEPTH: usize = 16;

#[derive(Clone, Copy, Debug, Default)]
struct ScreenKeyboardState {
    active: KittyKeyboardFlags,
    saved: [KittyKeyboardFlags; MAX_KEYBOARD_STACK_DEPTH],
    depth: usize,
}

#[derive(Debug, Default)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Csi(Vec<u8>),
    String,
    StringEscape,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ProtocolAudit {
    pub changes: u32,
    pub queries: u32,
    pub rejected: u32,
    pub unknown_flags: u32,
    pub stack_overflow: u32,
}

#[derive(Debug, Default)]
pub(super) struct KeyboardProtocolTracker {
    screens: [ScreenKeyboardState; 2],
    alternate: bool,
    modify_other_keys: ModifyOtherKeysLevel,
    scan: ScanState,
}

impl KeyboardProtocolTracker {
    pub fn state(&self) -> KeyboardProtocolState {
        KeyboardProtocolState {
            kitty: self.current().active,
            modify_other_keys: self.modify_other_keys,
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) -> (Vec<(usize, Vec<u8>)>, ProtocolAudit) {
        let mut replies = Vec::new();
        let mut audit = ProtocolAudit::default();
        for (offset, &byte) in bytes.iter().enumerate() {
            let state = std::mem::take(&mut self.scan);
            self.scan = match state {
                ScanState::Ground if byte == 0x1b => ScanState::Escape,
                ScanState::Ground if byte == 0x9b => ScanState::Csi(Vec::new()),
                ScanState::Escape if byte == b'[' => ScanState::Csi(Vec::new()),
                ScanState::Escape if matches!(byte, b']' | b'P' | b'^' | b'_') => ScanState::String,
                ScanState::Escape if byte == b'c' => {
                    self.reset();
                    audit.changes = audit.changes.saturating_add(1);
                    ScanState::Ground
                }
                ScanState::Escape if byte == 0x1b => ScanState::Escape,
                ScanState::String if matches!(byte, 0x07 | 0x9c) => ScanState::Ground,
                ScanState::StringEscape if byte == b'\\' => ScanState::Ground,
                ScanState::String | ScanState::StringEscape if byte == 0x1b => {
                    ScanState::StringEscape
                }
                ScanState::String | ScanState::StringEscape => ScanState::String,
                ScanState::Csi(sequence) if (0x40..=0x7e).contains(&byte) => {
                    self.handle_csi(&sequence, byte, offset, &mut replies, &mut audit);
                    ScanState::Ground
                }
                ScanState::Csi(mut sequence) if sequence.len() < MAX_SEQUENCE_BYTES => {
                    sequence.push(byte);
                    ScanState::Csi(sequence)
                }
                ScanState::Csi(_) => {
                    audit.rejected = audit.rejected.saturating_add(1);
                    ScanState::Ground
                }
                ScanState::Ground | ScanState::Escape => ScanState::Ground,
            };
        }
        (replies, audit)
    }

    fn handle_csi(
        &mut self,
        body: &[u8],
        final_byte: u8,
        offset: usize,
        replies: &mut Vec<(usize, Vec<u8>)>,
        audit: &mut ProtocolAudit,
    ) {
        if final_byte == b'u' && body == b"?" {
            replies.push((
                offset,
                format!("\x1b[?{}u", self.current().active.bits()).into_bytes(),
            ));
            audit.queries = audit.queries.saturating_add(1);
            return;
        }
        if final_byte == b'm' && body == b"?4" {
            replies.push((
                offset,
                format!("\x1b[>4;{}m", self.modify_other_keys as u8).into_bytes(),
            ));
            audit.queries = audit.queries.saturating_add(1);
            return;
        }
        let Some((&prefix, parameters)) = body.split_first() else {
            return;
        };
        let Some(parameters) = parse_parameters(parameters) else {
            audit.rejected = audit.rejected.saturating_add(1);
            return;
        };
        match (prefix, final_byte) {
            (b'=', b'u') => self.set_kitty(&parameters, audit),
            (b'>', b'u') => self.push_kitty(&parameters, audit),
            (b'<', b'u') => self.pop_kitty(&parameters, audit),
            (b'>', b'm') => self.set_mok(&parameters, audit),
            (b'?', b'h') => self.switch_screen(&parameters, true),
            (b'?', b'l') => self.switch_screen(&parameters, false),
            _ => {}
        }
    }

    fn set_kitty(&mut self, parameters: &[Option<u32>], audit: &mut ProtocolAudit) {
        let Some(flags) = validated_flags(parameter(parameters, 0, 0), audit) else {
            return;
        };
        let behavior = parameter(parameters, 1, 1);
        let current = self.current().active;
        let next = match behavior {
            1 => flags,
            2 => current.union(flags),
            3 => current.difference(flags),
            _ => {
                audit.rejected = audit.rejected.saturating_add(1);
                return;
            }
        };
        self.current_mut().active = next;
        audit.changes = audit.changes.saturating_add(1);
    }

    fn push_kitty(&mut self, parameters: &[Option<u32>], audit: &mut ProtocolAudit) {
        let Some(flags) = validated_flags(parameter(parameters, 0, 0), audit) else {
            return;
        };
        let current = self.current_mut();
        if current.depth == MAX_KEYBOARD_STACK_DEPTH {
            audit.stack_overflow = audit.stack_overflow.saturating_add(1);
            return;
        }
        current.saved[current.depth] = current.active;
        current.depth += 1;
        current.active = flags;
        audit.changes = audit.changes.saturating_add(1);
    }

    fn pop_kitty(&mut self, parameters: &[Option<u32>], audit: &mut ProtocolAudit) {
        let count = parameter(parameters, 0, 1);
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        let current = self.current_mut();
        if count > current.depth {
            current.depth = 0;
            current.active = KittyKeyboardFlags::default();
        } else if count != 0 {
            current.depth -= count;
            current.active = current.saved[current.depth];
        }
        audit.changes = audit.changes.saturating_add(1);
    }

    fn set_mok(&mut self, parameters: &[Option<u32>], audit: &mut ProtocolAudit) {
        if parameter(parameters, 0, 0) != 4 {
            return;
        }
        self.modify_other_keys = match parameter(parameters, 1, 0) {
            0 => ModifyOtherKeysLevel::Disabled,
            1 => ModifyOtherKeysLevel::ExceptWellDefined,
            2 => ModifyOtherKeysLevel::All,
            _ => {
                audit.rejected = audit.rejected.saturating_add(1);
                return;
            }
        };
        audit.changes = audit.changes.saturating_add(1);
    }

    fn switch_screen(&mut self, parameters: &[Option<u32>], alternate: bool) {
        if parameters
            .iter()
            .flatten()
            .any(|value| matches!(value, 47 | 1047 | 1049))
        {
            self.alternate = alternate;
        }
    }

    fn current(&self) -> &ScreenKeyboardState {
        &self.screens[usize::from(self.alternate)]
    }
    fn current_mut(&mut self) -> &mut ScreenKeyboardState {
        &mut self.screens[usize::from(self.alternate)]
    }

    pub(super) fn reset(&mut self) {
        self.screens = [ScreenKeyboardState::default(); 2];
        self.alternate = false;
        self.modify_other_keys = ModifyOtherKeysLevel::Disabled;
    }
}

fn validated_flags(value: u32, audit: &mut ProtocolAudit) -> Option<KittyKeyboardFlags> {
    let Ok(value) = u8::try_from(value) else {
        audit.unknown_flags = audit.unknown_flags.saturating_add(1);
        return None;
    };
    let flags = KittyKeyboardFlags::from_valid_bits(value);
    if flags.is_none() {
        audit.unknown_flags = audit.unknown_flags.saturating_add(1);
    }
    flags
}

fn parameter(parameters: &[Option<u32>], index: usize, default: u32) -> u32 {
    parameters.get(index).copied().flatten().unwrap_or(default)
}

fn parse_parameters(bytes: &[u8]) -> Option<Vec<Option<u32>>> {
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    bytes
        .split(|byte| *byte == b';')
        .map(|part| {
            if part.is_empty() {
                return Some(None);
            }
            part.iter()
                .try_fold(0_u32, |value, byte| {
                    let digit = byte.checked_sub(b'0').filter(|digit| *digit <= 9)?;
                    value.checked_mul(10)?.checked_add(u32::from(digit))
                })
                .map(Some)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_query_push_pop_and_unknown_flags_are_atomic() {
        let mut tracker = KeyboardProtocolTracker::default();
        let (reply, _) = tracker.advance(b"\x1b[=3;1u\x1b[?u");
        assert_eq!(reply, [(10, b"\x1b[?3u".to_vec())]);
        tracker.advance(b"\x1b[>8u\x1b[=1;2u\x1b[<1u");
        assert_eq!(tracker.state().kitty.bits(), 3);
        let (_, audit) = tracker.advance(b"\x1b[=32;1u");
        assert_eq!(audit.unknown_flags, 1);
        assert_eq!(tracker.state().kitty.bits(), 3);
    }

    #[test]
    fn state_is_per_screen_and_ris_resets_everything() {
        let mut tracker = KeyboardProtocolTracker::default();
        tracker.advance(b"\x1b[=1u\x1b[?1049h\x1b[=8u");
        assert_eq!(tracker.state().kitty.bits(), 8);
        tracker.advance(b"\x1b[?1049l");
        assert_eq!(tracker.state().kitty.bits(), 1);
        tracker.advance(b"\x1b[>4;2m\x1bc");
        assert_eq!(tracker.state(), KeyboardProtocolState::default());
    }

    #[test]
    fn vim_omitted_mok_level_disables_protocol() {
        let mut tracker = KeyboardProtocolTracker::default();
        tracker.advance(b"\x1b[>4;2m");
        assert_eq!(tracker.state().modify_other_keys, ModifyOtherKeysLevel::All);

        let (_, audit) = tracker.advance(b"\x1b[>4;m");
        assert_eq!(
            tracker.state().modify_other_keys,
            ModifyOtherKeysLevel::Disabled
        );
        assert_eq!(audit.changes, 1);
        assert_eq!(audit.rejected, 0);
    }

    #[test]
    fn fragmented_sequences_and_stack_overflow_are_bounded() {
        let mut tracker = KeyboardProtocolTracker::default();
        tracker.advance(b"\x1b[");
        tracker.advance(b"=7;1u");
        for _ in 0..16 {
            tracker.advance(b"\x1b[>1u");
        }
        let (_, audit) = tracker.advance(b"\x1b[>2u");
        assert_eq!(audit.stack_overflow, 1);
        assert_eq!(tracker.state().kitty.bits(), 1);
    }
}

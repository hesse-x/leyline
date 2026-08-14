//! Stable input values exported by the native platform boundary.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LogicalKey {
    Character(char),
    Backspace,
    Tab,
    Enter,
    Escape,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Function(u8),
    Unidentified(u32),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SeatToken {
    slot: u32,
    generation: u32,
}

impl SeatToken {
    #[must_use]
    pub const fn new(slot: u32, generation: u32) -> Self {
        Self { slot, generation }
    }

    #[must_use]
    pub const fn next_generation(self) -> Self {
        Self {
            slot: self.slot,
            generation: self.generation.wrapping_add(1),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SerialKind {
    Keyboard,
    Pointer,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InputSerial {
    pub seat: SeatToken,
    pub value: u32,
    pub kind: SerialKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ModifierMask(u8);

impl ModifierMask {
    pub const SHIFT: Self = Self(1 << 0);
    pub const CONTROL: Self = Self(1 << 1);
    pub const ALT: Self = Self(1 << 2);
    pub const SUPER: Self = Self(1 << 3);

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// Maps xkb's stable numeric keysyms to the application's logical key vocabulary.
#[must_use]
pub fn logical_key_from_keysym(keysym: u32) -> LogicalKey {
    match keysym {
        0xff08 => LogicalKey::Backspace,
        0xff09 | 0xfe20 => LogicalKey::Tab,
        0xff0d | 0xff8d => LogicalKey::Enter,
        0xff1b => LogicalKey::Escape,
        0xff50 => LogicalKey::Home,
        0xff51 => LogicalKey::ArrowLeft,
        0xff52 => LogicalKey::ArrowUp,
        0xff53 => LogicalKey::ArrowRight,
        0xff54 => LogicalKey::ArrowDown,
        0xff55 => LogicalKey::PageUp,
        0xff56 => LogicalKey::PageDown,
        0xff57 => LogicalKey::End,
        0xff63 => LogicalKey::Insert,
        0xffff => LogicalKey::Delete,
        0xffbe..=0xffc9 => LogicalKey::Function(u8::try_from(keysym - 0xffbd).unwrap_or_default()),
        _ => {
            keysym_character(keysym).map_or(LogicalKey::Unidentified(keysym), LogicalKey::Character)
        }
    }
}

#[must_use]
pub fn keysym_character(keysym: u32) -> Option<char> {
    let ch = if matches!(keysym, 0x20..=0x7e | 0xa0..=0xff) {
        char::from_u32(keysym)
    } else if matches!(keysym, 0x0100_0100..=0x0110_ffff) {
        char::from_u32(keysym - 0x0100_0000)
    } else {
        None
    }?;
    (!ch.is_control()).then_some(ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_keysym_mapping_covers_named_unicode_and_unknown_keys() {
        assert_eq!(logical_key_from_keysym(0xff63), LogicalKey::Insert);
        assert_eq!(logical_key_from_keysym(0xff55), LogicalKey::PageUp);
        assert_eq!(logical_key_from_keysym(0xffc9), LogicalKey::Function(12));
        assert_eq!(
            logical_key_from_keysym(0x0100_4e2d),
            LogicalKey::Character('中')
        );
        assert_eq!(
            logical_key_from_keysym(0xdead_beef),
            LogicalKey::Unidentified(0xdead_beef)
        );
    }
}

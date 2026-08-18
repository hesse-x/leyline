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
    Modifier { kind: ModifierKind, side: KeySide },
    CapsLock,
    NumLock,
    Menu,
    Unidentified(u32),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KeyLocation {
    Standard,
    Numpad,
    Left,
    Right,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KeySide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModifierKind {
    Shift,
    Control,
    Alt,
    Super,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KeypadKey {
    Digit(u8),
    Decimal,
    Separator,
    Add,
    Subtract,
    Multiply,
    Divide,
    Equal,
    Enter,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyIdentity {
    pub logical: LogicalKey,
    pub location: KeyLocation,
    pub keypad: Option<KeypadKey>,
    pub base_codepoint: Option<char>,
    pub shifted_codepoint: Option<char>,
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
        0xffbe..=0xffe0 => LogicalKey::Function(u8::try_from(keysym - 0xffbd).unwrap_or_default()),
        0xffe1 => LogicalKey::Modifier {
            kind: ModifierKind::Shift,
            side: KeySide::Left,
        },
        0xffe2 => LogicalKey::Modifier {
            kind: ModifierKind::Shift,
            side: KeySide::Right,
        },
        0xffe3 => LogicalKey::Modifier {
            kind: ModifierKind::Control,
            side: KeySide::Left,
        },
        0xffe4 => LogicalKey::Modifier {
            kind: ModifierKind::Control,
            side: KeySide::Right,
        },
        0xffe5 => LogicalKey::CapsLock,
        0xffe7 | 0xffeb => LogicalKey::Modifier {
            kind: ModifierKind::Super,
            side: KeySide::Left,
        },
        0xffe8 | 0xffec => LogicalKey::Modifier {
            kind: ModifierKind::Super,
            side: KeySide::Right,
        },
        0xffe9 => LogicalKey::Modifier {
            kind: ModifierKind::Alt,
            side: KeySide::Left,
        },
        0xffea | 0xfe03 => LogicalKey::Modifier {
            kind: ModifierKind::Alt,
            side: KeySide::Right,
        },
        0xff7f => LogicalKey::NumLock,
        0xff67 => LogicalKey::Menu,
        _ => {
            keysym_character(keysym).map_or(LogicalKey::Unidentified(keysym), LogicalKey::Character)
        }
    }
}

/// Classifies stable XKB keypad keysyms without consulting an evdev keycode table.
#[must_use]
pub fn keypad_key_from_keysym(keysym: u32) -> Option<KeypadKey> {
    match keysym {
        0xffb0..=0xffb9 => Some(KeypadKey::Digit(u8::try_from(keysym - 0xffb0).ok()?)),
        0xffae => Some(KeypadKey::Decimal),
        0xffac => Some(KeypadKey::Separator),
        0xffab => Some(KeypadKey::Add),
        0xffad => Some(KeypadKey::Subtract),
        0xffaa => Some(KeypadKey::Multiply),
        0xffaf => Some(KeypadKey::Divide),
        0xffbd => Some(KeypadKey::Equal),
        0xff8d => Some(KeypadKey::Enter),
        0xff95 => Some(KeypadKey::Home),
        0xff9c => Some(KeypadKey::End),
        0xff9a => Some(KeypadKey::PageUp),
        0xff9b => Some(KeypadKey::PageDown),
        0xff9e => Some(KeypadKey::Insert),
        0xff9f => Some(KeypadKey::Delete),
        0xff97 => Some(KeypadKey::ArrowUp),
        0xff99 => Some(KeypadKey::ArrowDown),
        0xff96 => Some(KeypadKey::ArrowLeft),
        0xff98 => Some(KeypadKey::ArrowRight),
        _ => None,
    }
}

#[must_use]
pub fn key_identity_from_keysym(keysym: u32) -> KeyIdentity {
    let logical = logical_key_from_keysym(keysym);
    let keypad = keypad_key_from_keysym(keysym);
    let location = if keypad.is_some() {
        KeyLocation::Numpad
    } else {
        match logical {
            LogicalKey::Modifier {
                side: KeySide::Left,
                ..
            } => KeyLocation::Left,
            LogicalKey::Modifier {
                side: KeySide::Right,
                ..
            } => KeyLocation::Right,
            LogicalKey::Unidentified(_) => KeyLocation::Unknown,
            _ => KeyLocation::Standard,
        }
    };
    let base_codepoint = keysym_character(keysym);
    KeyIdentity {
        logical,
        location,
        keypad,
        base_codepoint,
        shifted_codepoint: None,
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

    #[test]
    fn keypad_and_modifier_locations_are_stable() {
        assert_eq!(
            key_identity_from_keysym(0xffb7).keypad,
            Some(KeypadKey::Digit(7))
        );
        assert_eq!(
            key_identity_from_keysym(0xff8d).location,
            KeyLocation::Numpad
        );
        assert_eq!(
            key_identity_from_keysym(0xffe4).location,
            KeyLocation::Right
        );
        assert_eq!(logical_key_from_keysym(0xffdc), LogicalKey::Function(31));
    }
}

use leyline_gfx::{KeyInput, KeyState, LogicalKey, ModifierMask};

use crate::config::Action;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LogicalKeyPattern {
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
}

impl LogicalKeyPattern {
    #[must_use]
    pub fn matches(self, key: LogicalKey) -> bool {
        match (self, key) {
            (Self::Character(expected), LogicalKey::Character(actual))
                if expected.is_ascii() && actual.is_ascii() =>
            {
                expected.eq_ignore_ascii_case(&actual)
            }
            (Self::Character(expected), LogicalKey::Character(actual)) => expected == actual,
            (Self::Backspace, LogicalKey::Backspace)
            | (Self::Tab, LogicalKey::Tab)
            | (Self::Enter, LogicalKey::Enter)
            | (Self::Escape, LogicalKey::Escape)
            | (Self::Insert, LogicalKey::Insert)
            | (Self::Delete, LogicalKey::Delete)
            | (Self::Home, LogicalKey::Home)
            | (Self::End, LogicalKey::End)
            | (Self::PageUp, LogicalKey::PageUp)
            | (Self::PageDown, LogicalKey::PageDown)
            | (Self::ArrowUp, LogicalKey::ArrowUp)
            | (Self::ArrowDown, LogicalKey::ArrowDown)
            | (Self::ArrowLeft, LogicalKey::ArrowLeft)
            | (Self::ArrowRight, LogicalKey::ArrowRight) => true,
            (Self::Function(expected), LogicalKey::Function(actual)) => expected == actual,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyChord {
    pub key: LogicalKeyPattern,
    pub modifiers: ModifierMask,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingOrigin {
    Default,
    User { index: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutResult {
    Matched(Action),
    NotMatched,
}

#[must_use]
pub fn resolve(bindings: &[crate::config::KeyBinding], key: &KeyInput) -> ShortcutResult {
    if key.state == KeyState::Released {
        return ShortcutResult::NotMatched;
    }
    bindings
        .iter()
        .rev()
        .find(|binding| {
            binding.chord.modifiers == key.shortcut_modifiers
                && (binding.chord.key.matches(key.logical_key)
                    || matches!(binding.origin, BindingOrigin::Default)
                        && matches!(binding.action, Action::ActivateTab(number) if key.shortcut_digit_row.is_some_and(|digit| digit.get() == number)))
        })
        .map_or(ShortcutResult::NotMatched, |binding| {
            ShortcutResult::Matched(binding.action)
        })
}

#[must_use]
pub fn resolve_with_terminal_gesture(
    bindings: &[crate::config::KeyBinding],
    key: &KeyInput,
    terminal_control_gesture: bool,
) -> ShortcutResult {
    if terminal_control_gesture {
        ShortcutResult::NotMatched
    } else {
        resolve(bindings, key)
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
#[error("unknown logical key")]
pub struct ParseKeyError;

/// Parses the finite configuration key vocabulary or one Unicode scalar.
///
/// # Errors
/// Returns [`ParseKeyError`] for unknown names, control characters, or multiple scalars.
pub fn parse_key(value: &str) -> Result<LogicalKeyPattern, ParseKeyError> {
    let named = match value.to_ascii_lowercase().as_str() {
        "backspace" => Some(LogicalKeyPattern::Backspace),
        "tab" => Some(LogicalKeyPattern::Tab),
        "enter" | "return" => Some(LogicalKeyPattern::Enter),
        "escape" | "esc" => Some(LogicalKeyPattern::Escape),
        "insert" => Some(LogicalKeyPattern::Insert),
        "delete" => Some(LogicalKeyPattern::Delete),
        "home" => Some(LogicalKeyPattern::Home),
        "end" => Some(LogicalKeyPattern::End),
        "pageup" | "page_up" => Some(LogicalKeyPattern::PageUp),
        "pagedown" | "page_down" => Some(LogicalKeyPattern::PageDown),
        "up" => Some(LogicalKeyPattern::ArrowUp),
        "down" => Some(LogicalKeyPattern::ArrowDown),
        "left" => Some(LogicalKeyPattern::ArrowLeft),
        "right" => Some(LogicalKeyPattern::ArrowRight),
        "plus" => Some(LogicalKeyPattern::Character('+')),
        "minus" => Some(LogicalKeyPattern::Character('-')),
        _ => None,
    };
    if let Some(key) = named {
        return Ok(key);
    }
    if let Some(number) = value
        .strip_prefix('F')
        .or_else(|| value.strip_prefix('f'))
        .and_then(|number| number.parse::<u8>().ok())
        && (1..=12).contains(&number)
    {
        return Ok(LogicalKeyPattern::Function(number));
    }
    let mut chars = value.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if !ch.is_control() => Ok(LogicalKeyPattern::Character(ch)),
        _ => Err(ParseKeyError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(logical_key: LogicalKey, digit: Option<u8>) -> KeyInput {
        let mut shortcut_modifiers = ModifierMask::CONTROL;
        shortcut_modifiers.insert(ModifierMask::SHIFT);
        KeyInput {
            serial: leyline_gfx::InputSerial {
                seat: leyline_gfx::SeatToken::new(0, 1),
                value: 1,
                kind: leyline_gfx::SerialKind::Keyboard,
            },
            time_ms: 1,
            physical_keycode: 2,
            shortcut_digit_row: digit.and_then(std::num::NonZeroU8::new),
            utf8: None,
            modifiers: leyline_gfx::ModifiersState::default(),
            shortcut_modifiers,
            logical_key,
            state: KeyState::Pressed,
            repeat: false,
        }
    }

    #[test]
    fn parser_has_finite_named_vocabulary_and_unicode_scalars() {
        assert_eq!(parse_key("Insert"), Ok(LogicalKeyPattern::Insert));
        assert_eq!(parse_key("plus"), Ok(LogicalKeyPattern::Character('+')));
        assert_eq!(parse_key("中"), Ok(LogicalKeyPattern::Character('中')));
        assert_eq!(parse_key("not-a-key"), Err(ParseKeyError));
        assert_eq!(parse_key("F13"), Err(ParseKeyError));
    }

    #[test]
    fn default_digit_action_uses_xkb_row_without_stealing_user_character_binding() {
        let mut chord_modifiers = ModifierMask::CONTROL;
        chord_modifiers.insert(ModifierMask::SHIFT);
        let default = crate::config::KeyBinding {
            chord: KeyChord {
                key: LogicalKeyPattern::Character('1'),
                modifiers: chord_modifiers,
            },
            action: Action::ActivateTab(1),
            origin: BindingOrigin::Default,
        };
        assert_eq!(
            resolve(
                std::slice::from_ref(&default),
                &input(LogicalKey::Character('!'), Some(1))
            ),
            ShortcutResult::Matched(Action::ActivateTab(1))
        );
        let user = crate::config::KeyBinding {
            chord: KeyChord {
                key: LogicalKeyPattern::Character('!'),
                modifiers: chord_modifiers,
            },
            action: Action::ScrollPageUp,
            origin: BindingOrigin::User { index: 0 },
        };
        assert_eq!(
            resolve(
                &[default.clone(), user],
                &input(LogicalKey::Character('!'), Some(1))
            ),
            ShortcutResult::Matched(Action::ScrollPageUp)
        );

        let tab_five = crate::config::KeyBinding {
            chord: KeyChord {
                key: LogicalKeyPattern::Character('5'),
                modifiers: chord_modifiers,
            },
            action: Action::ActivateTab(5),
            origin: BindingOrigin::Default,
        };
        let shifted_five = input(LogicalKey::Character('%'), Some(5));
        assert_eq!(
            resolve(std::slice::from_ref(&tab_five), &shifted_five),
            ShortcutResult::Matched(Action::ActivateTab(5))
        );
        assert_eq!(
            resolve_with_terminal_gesture(std::slice::from_ref(&tab_five), &shifted_five, true),
            ShortcutResult::NotMatched
        );
    }
}

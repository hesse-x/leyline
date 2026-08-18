use super::{KittyKeyboardFlags, ModifyOtherKeysLevel};
use super::{MouseEncoding, MouseProtocol, TerminalModes};
use leyline_gfx::{KeyIdentity, KeyLocation, KeySide, KeypadKey, LogicalKey, ModifierKind};

const MAX_COMMIT_BYTES: usize = 64 * 1024;
pub const MAX_TRANSACTION_BYTES: usize = 1024 * 1024;
pub const MAX_PASTE_BODY_BYTES: usize = MAX_TRANSACTION_BYTES - 12;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalKey {
    Backspace,
    Tab,
    Enter,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    Function(u8),
    Char(char),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardEventKind {
    Press,
    Repeat,
    Release,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalKeyboardEvent {
    pub identity: KeyIdentity,
    pub text: Option<String>,
    pub modifiers: Modifiers,
    pub caps_lock: bool,
    pub num_lock: bool,
    pub kind: KeyboardEventKind,
    pub associated_text_allowed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EncodedKey {
    Bytes(Vec<u8>),
    TextFallback,
    Ignored(IgnoreReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgnoreReason {
    ReleaseNotReported,
    UnknownKey,
}

const MAX_ENCODED_KEY_BYTES: usize = 256;
const MAX_ASSOCIATED_TEXT_BYTES: usize = 64;
const MAX_ASSOCIATED_TEXT_SCALARS: usize = 16;

#[allow(clippy::missing_errors_doc)]
pub fn encode_keyboard_event(
    event: &TerminalKeyboardEvent,
    modes: TerminalModes,
) -> Result<EncodedKey, InputError> {
    let kitty = modes.keyboard.kitty;
    if !kitty.is_empty() || matches!(event.identity.logical, LogicalKey::Function(13..=35)) {
        return encode_kitty(event, kitty, modes);
    }
    if event.kind == KeyboardEventKind::Release {
        return Ok(EncodedKey::Ignored(IgnoreReason::ReleaseNotReported));
    }
    if let Some(encoded) = encode_modify_other_keys(event, modes.keyboard.modify_other_keys)? {
        return Ok(EncodedKey::Bytes(encoded));
    }
    if event.identity.location == KeyLocation::Numpad {
        return encode_keypad(event, modes);
    }
    let Some(key) = terminal_key(event.identity.logical) else {
        return if event.text.is_some() {
            Ok(EncodedKey::TextFallback)
        } else {
            Ok(EncodedKey::Ignored(IgnoreReason::UnknownKey))
        };
    };
    if matches!(key, TerminalKey::Char(_)) && !event.modifiers.control && !event.modifiers.alt {
        return Ok(EncodedKey::TextFallback);
    }
    Ok(EncodedKey::Bytes(encode_key(key, event.modifiers, modes)?))
}

fn encode_kitty(
    event: &TerminalKeyboardEvent,
    flags: KittyKeyboardFlags,
    modes: TerminalModes,
) -> Result<EncodedKey, InputError> {
    let text_key = matches!(event.identity.logical, LogicalKey::Character(_));
    let legacy_functional = terminal_key(event.identity.logical).filter(|key| {
        matches!(
            key,
            TerminalKey::Up
                | TerminalKey::Down
                | TerminalKey::Left
                | TerminalKey::Right
                | TerminalKey::Home
                | TerminalKey::End
                | TerminalKey::Insert
                | TerminalKey::Delete
                | TerminalKey::PageUp
                | TerminalKey::PageDown
                | TerminalKey::Function(1..=12)
        )
    });
    if event.identity.location != KeyLocation::Numpad
        && let Some(key) = legacy_functional
    {
        if flags.contains(KittyKeyboardFlags::REPORT_EVENTS) {
            return encode_kitty_legacy_functional(key, event).map(EncodedKey::Bytes);
        }
        if event.kind == KeyboardEventKind::Release {
            return Ok(EncodedKey::Ignored(IgnoreReason::ReleaseNotReported));
        }
        return encode_key(key, event.modifiers, modes).map(EncodedKey::Bytes);
    }
    let reset_key = matches!(
        event.identity.logical,
        LogicalKey::Backspace | LogicalKey::Tab | LogicalKey::Enter
    );
    if reset_key
        && event.kind == KeyboardEventKind::Release
        && !flags.contains(KittyKeyboardFlags::ALL_KEYS)
    {
        return Ok(EncodedKey::Ignored(IgnoreReason::ReleaseNotReported));
    }
    let special_disambiguated = matches!(
        event.identity.logical,
        LogicalKey::Backspace | LogicalKey::Tab | LogicalKey::Enter | LogicalKey::Escape
    ) || (text_key && (event.modifiers.alt || event.modifiers.control));
    let use_csi = flags.contains(KittyKeyboardFlags::ALL_KEYS)
        || event.identity.location == KeyLocation::Numpad
        || matches!(event.identity.logical, LogicalKey::Function(13..=35))
        || (!reset_key && !text_key && flags.contains(KittyKeyboardFlags::REPORT_EVENTS))
        || (flags.contains(KittyKeyboardFlags::DISAMBIGUATE) && special_disambiguated);
    if !use_csi {
        if event.kind == KeyboardEventKind::Release {
            return Ok(EncodedKey::Ignored(IgnoreReason::ReleaseNotReported));
        }
        if text_key && !event.modifiers.alt && !event.modifiers.control {
            return Ok(EncodedKey::TextFallback);
        }
        let Some(key) = terminal_key(event.identity.logical) else {
            return Ok(EncodedKey::Ignored(IgnoreReason::UnknownKey));
        };
        return Ok(EncodedKey::Bytes(encode_key(key, event.modifiers, modes)?));
    }
    if event.kind == KeyboardEventKind::Release
        && !flags.contains(KittyKeyboardFlags::REPORT_EVENTS)
    {
        return Ok(EncodedKey::Ignored(IgnoreReason::ReleaseNotReported));
    }
    let Some(code) = kitty_key_code(&event.identity) else {
        return Ok(EncodedKey::Ignored(IgnoreReason::UnknownKey));
    };
    let mut output = format!("\x1b[{code}");
    append_kitty_alternate_keys(&mut output, event, flags);
    output.push(';');
    output.push_str(&kitty_modifiers(event).to_string());
    if flags.contains(KittyKeyboardFlags::REPORT_EVENTS) {
        output.push(':');
        output.push(char::from(b'0' + kitty_event_type(event.kind)));
    }
    append_kitty_associated_text(&mut output, event, flags)?;
    output.push('u');
    checked_key_bytes(output.into_bytes()).map(EncodedKey::Bytes)
}

fn append_kitty_alternate_keys(
    output: &mut String,
    event: &TerminalKeyboardEvent,
    flags: KittyKeyboardFlags,
) {
    if !flags.contains(KittyKeyboardFlags::ALTERNATE_KEYS) {
        return;
    }
    let shifted = event
        .modifiers
        .shift
        .then_some(event.identity.shifted_codepoint)
        .flatten();
    let base = event.identity.base_codepoint;
    if shifted.is_some() || base.is_some() {
        output.push(':');
    }
    if let Some(shifted) = shifted {
        push_codepoint(output, shifted);
    }
    if let Some(base) = base {
        output.push(':');
        push_codepoint(output, base);
    }
}

fn append_kitty_associated_text(
    output: &mut String,
    event: &TerminalKeyboardEvent,
    flags: KittyKeyboardFlags,
) -> Result<(), InputError> {
    if !flags.contains(KittyKeyboardFlags::ASSOCIATED_TEXT)
        || !event.associated_text_allowed
        || event.kind == KeyboardEventKind::Release
    {
        return Ok(());
    }
    let Some(text) = event.text.as_deref() else {
        return Ok(());
    };
    if text.len() > MAX_ASSOCIATED_TEXT_BYTES || text.chars().count() > MAX_ASSOCIATED_TEXT_SCALARS
    {
        return Err(InputError::AssociatedTextCapacity);
    }
    if text
        .chars()
        .any(|ch| ch <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&ch))
    {
        return Err(InputError::AssociatedTextControl);
    }
    if !text.is_empty() {
        output.push(';');
        for (index, ch) in text.chars().enumerate() {
            if index != 0 {
                output.push(':');
            }
            push_codepoint(output, ch);
        }
    }
    Ok(())
}

const fn kitty_event_type(kind: KeyboardEventKind) -> u8 {
    match kind {
        KeyboardEventKind::Press => 1,
        KeyboardEventKind::Repeat => 2,
        KeyboardEventKind::Release => 3,
    }
}

fn encode_kitty_legacy_functional(
    key: TerminalKey,
    event: &TerminalKeyboardEvent,
) -> Result<Vec<u8>, InputError> {
    let modifiers = kitty_modifiers(event);
    let kind = kitty_event_type(event.kind);
    let suffix = format!("{modifiers}:{kind}");
    let output = match key {
        TerminalKey::Up => format!("\x1b[1;{suffix}A"),
        TerminalKey::Down => format!("\x1b[1;{suffix}B"),
        TerminalKey::Right => format!("\x1b[1;{suffix}C"),
        TerminalKey::Left => format!("\x1b[1;{suffix}D"),
        TerminalKey::Home => format!("\x1b[1;{suffix}H"),
        TerminalKey::End => format!("\x1b[1;{suffix}F"),
        TerminalKey::Insert => format!("\x1b[2;{suffix}~"),
        TerminalKey::Delete => format!("\x1b[3;{suffix}~"),
        TerminalKey::PageUp => format!("\x1b[5;{suffix}~"),
        TerminalKey::PageDown => format!("\x1b[6;{suffix}~"),
        TerminalKey::Function(number @ 1..=4) => {
            let final_byte = char::from(b'P' + number - 1);
            format!("\x1b[1;{suffix}{final_byte}")
        }
        TerminalKey::Function(number @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(number - 5)];
            format!("\x1b[{code};{suffix}~")
        }
        _ => return Err(InputError::UnknownKey),
    };
    checked_key_bytes(output.into_bytes())
}

fn encode_modify_other_keys(
    event: &TerminalKeyboardEvent,
    level: ModifyOtherKeysLevel,
) -> Result<Option<Vec<u8>>, InputError> {
    if level == ModifyOtherKeysLevel::Disabled || event.kind == KeyboardEventKind::Release {
        return Ok(None);
    }
    let LogicalKey::Character(ch) = event.identity.logical else {
        return Ok(None);
    };
    let modified = event.modifiers.shift || event.modifiers.alt || event.modifiers.control;
    if !modified {
        return Ok(None);
    }
    let base = event.identity.base_codepoint.unwrap_or(ch);
    let well_defined = !event.modifiers.alt
        && ((event.modifiers.shift && !event.modifiers.control)
            || (event.modifiers.control
                && (matches!(ch, '@'..='~' | ' ')
                    || (!event.modifiers.shift && matches!(base, '2'..='8')))));
    if level == ModifyOtherKeysLevel::ExceptWellDefined && well_defined {
        return Ok(None);
    }
    let modifier_code = modifier_parameter(event.modifiers);
    checked_key_bytes(format!("\x1b[27;{modifier_code};{}~", u32::from(ch)).into_bytes()).map(Some)
}

fn encode_keypad(
    event: &TerminalKeyboardEvent,
    modes: TerminalModes,
) -> Result<EncodedKey, InputError> {
    let Some(keypad) = event.identity.keypad else {
        return if event.text.is_some() {
            Ok(EncodedKey::TextFallback)
        } else {
            Ok(EncodedKey::Ignored(IgnoreReason::UnknownKey))
        };
    };
    if !modes.application_keypad {
        if event.text.is_some() {
            return Ok(EncodedKey::TextFallback);
        }
        if let Some(key) = terminal_key(event.identity.logical)
            && !matches!(key, TerminalKey::Char(_) | TerminalKey::Function(_))
        {
            return Ok(EncodedKey::Bytes(encode_key(key, event.modifiers, modes)?));
        }
        if let Some(key) = keypad_navigation(keypad) {
            return Ok(EncodedKey::Bytes(encode_key(key, event.modifiers, modes)?));
        }
        return Ok(EncodedKey::Ignored(IgnoreReason::UnknownKey));
    }
    let final_byte = match keypad {
        KeypadKey::Digit(digit @ 0..=9) => b'p' + digit,
        KeypadKey::Decimal => b'n',
        KeypadKey::Divide => b'o',
        KeypadKey::Multiply => b'j',
        KeypadKey::Subtract => b'm',
        KeypadKey::Add => b'k',
        KeypadKey::Separator => b'l',
        KeypadKey::Equal => b'X',
        KeypadKey::Enter => b'M',
        navigation => {
            if let Some(key) = keypad_navigation(navigation) {
                return Ok(EncodedKey::Bytes(encode_key(key, event.modifiers, modes)?));
            }
            return Ok(EncodedKey::Ignored(IgnoreReason::UnknownKey));
        }
    };
    let parameter = modifier_parameter(event.modifiers);
    let bytes = if parameter == 1 {
        vec![0x1b, b'O', final_byte]
    } else {
        format!("\x1b[1;{parameter}{}", char::from(final_byte)).into_bytes()
    };
    Ok(EncodedKey::Bytes(bytes))
}

fn keypad_navigation(key: KeypadKey) -> Option<TerminalKey> {
    Some(match key {
        KeypadKey::Home => TerminalKey::Home,
        KeypadKey::End => TerminalKey::End,
        KeypadKey::PageUp => TerminalKey::PageUp,
        KeypadKey::PageDown => TerminalKey::PageDown,
        KeypadKey::Insert => TerminalKey::Insert,
        KeypadKey::Delete => TerminalKey::Delete,
        KeypadKey::ArrowUp => TerminalKey::Up,
        KeypadKey::ArrowDown => TerminalKey::Down,
        KeypadKey::ArrowLeft => TerminalKey::Left,
        KeypadKey::ArrowRight => TerminalKey::Right,
        _ => return None,
    })
}

fn terminal_key(key: LogicalKey) -> Option<TerminalKey> {
    Some(match key {
        LogicalKey::Backspace => TerminalKey::Backspace,
        LogicalKey::Tab => TerminalKey::Tab,
        LogicalKey::Enter => TerminalKey::Enter,
        LogicalKey::Escape => TerminalKey::Escape,
        LogicalKey::Insert => TerminalKey::Insert,
        LogicalKey::Delete => TerminalKey::Delete,
        LogicalKey::Home => TerminalKey::Home,
        LogicalKey::End => TerminalKey::End,
        LogicalKey::PageUp => TerminalKey::PageUp,
        LogicalKey::PageDown => TerminalKey::PageDown,
        LogicalKey::ArrowUp => TerminalKey::Up,
        LogicalKey::ArrowDown => TerminalKey::Down,
        LogicalKey::ArrowLeft => TerminalKey::Left,
        LogicalKey::ArrowRight => TerminalKey::Right,
        LogicalKey::Function(number) => TerminalKey::Function(number),
        LogicalKey::Character(ch) => TerminalKey::Char(ch),
        _ => return None,
    })
}

fn kitty_key_code(identity: &KeyIdentity) -> Option<u32> {
    if let Some(keypad) = identity.keypad {
        return Some(match keypad {
            KeypadKey::Digit(digit @ 0..=9) => 57_399 + u32::from(digit),
            KeypadKey::Decimal => 57_409,
            KeypadKey::Divide => 57_410,
            KeypadKey::Multiply => 57_411,
            KeypadKey::Subtract => 57_412,
            KeypadKey::Add => 57_413,
            KeypadKey::Enter => 57_414,
            KeypadKey::Equal => 57_415,
            KeypadKey::Separator => 57_416,
            KeypadKey::ArrowLeft => 57_417,
            KeypadKey::ArrowRight => 57_418,
            KeypadKey::ArrowUp => 57_419,
            KeypadKey::ArrowDown => 57_420,
            KeypadKey::PageUp => 57_421,
            KeypadKey::PageDown => 57_422,
            KeypadKey::Home => 57_423,
            KeypadKey::End => 57_424,
            KeypadKey::Insert => 57_425,
            KeypadKey::Delete => 57_426,
            KeypadKey::Digit(_) => return None,
        });
    }
    Some(match identity.logical {
        LogicalKey::Character(ch) => u32::from(identity.base_codepoint.unwrap_or(ch)),
        LogicalKey::Escape => 27,
        LogicalKey::Enter => 13,
        LogicalKey::Tab => 9,
        LogicalKey::Backspace => 127,
        LogicalKey::Insert => 57_348,
        LogicalKey::Delete => 57_349,
        LogicalKey::ArrowLeft => 57_350,
        LogicalKey::ArrowRight => 57_351,
        LogicalKey::ArrowUp => 57_352,
        LogicalKey::ArrowDown => 57_353,
        LogicalKey::PageUp => 57_354,
        LogicalKey::PageDown => 57_355,
        LogicalKey::Home => 57_356,
        LogicalKey::End => 57_357,
        LogicalKey::CapsLock => 57_358,
        LogicalKey::NumLock => 57_360,
        LogicalKey::Menu => 57_363,
        LogicalKey::Function(number @ 1..=35) => 57_363 + u32::from(number),
        LogicalKey::Modifier { kind, side } => match (kind, side) {
            (ModifierKind::Shift, KeySide::Left) => 57_441,
            (ModifierKind::Control, KeySide::Left) => 57_442,
            (ModifierKind::Alt, KeySide::Left) => 57_443,
            (ModifierKind::Super, KeySide::Left) => 57_444,
            (ModifierKind::Shift, KeySide::Right) => 57_447,
            (ModifierKind::Control, KeySide::Right) => 57_448,
            (ModifierKind::Alt, KeySide::Right) => 57_449,
            (ModifierKind::Super, KeySide::Right) => 57_450,
        },
        _ => return None,
    })
}

fn kitty_modifiers(event: &TerminalKeyboardEvent) -> u16 {
    1 + u16::from(event.modifiers.shift)
        + 2 * u16::from(event.modifiers.alt)
        + 4 * u16::from(event.modifiers.control)
        + 8 * u16::from(event.modifiers.super_key)
        + 64 * u16::from(event.caps_lock)
        + 128 * u16::from(event.num_lock)
}

fn push_codepoint(output: &mut String, ch: char) {
    output.push_str(&u32::from(ch).to_string());
}

fn checked_key_bytes(bytes: Vec<u8>) -> Result<Vec<u8>, InputError> {
    if bytes.len() > MAX_ENCODED_KEY_BYTES {
        Err(InputError::EncodedKeyCapacity)
    } else {
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonState {
    Pressed,
    Released,
    Motion,
}

#[allow(clippy::missing_errors_doc)]
pub fn encode_key(
    key: TerminalKey,
    modifiers: Modifiers,
    modes: TerminalModes,
) -> Result<Vec<u8>, InputError> {
    let alt_is_parameter = matches!(
        key,
        TerminalKey::Up
            | TerminalKey::Down
            | TerminalKey::Left
            | TerminalKey::Right
            | TerminalKey::Home
            | TerminalKey::End
            | TerminalKey::Insert
            | TerminalKey::Delete
            | TerminalKey::PageUp
            | TerminalKey::PageDown
            | TerminalKey::Function(_)
    );
    let mut encoded = match key {
        TerminalKey::Char(ch) => encode_char(ch, modifiers),
        TerminalKey::Backspace => vec![0x7f],
        TerminalKey::Tab if modifiers.shift => b"\x1b[Z".to_vec(),
        TerminalKey::Tab => vec![b'\t'],
        TerminalKey::Enter => vec![b'\r'],
        TerminalKey::Escape => vec![0x1b],
        TerminalKey::Up => cursor(b'A', modes.application_cursor, modifiers),
        TerminalKey::Down => cursor(b'B', modes.application_cursor, modifiers),
        TerminalKey::Right => cursor(b'C', modes.application_cursor, modifiers),
        TerminalKey::Left => cursor(b'D', modes.application_cursor, modifiers),
        TerminalKey::Home => cursor(b'H', modes.application_cursor, modifiers),
        TerminalKey::End => cursor(b'F', modes.application_cursor, modifiers),
        TerminalKey::Insert => tilde(2, modifiers),
        TerminalKey::Delete => tilde(3, modifiers),
        TerminalKey::PageUp => tilde(5, modifiers),
        TerminalKey::PageDown => tilde(6, modifiers),
        TerminalKey::Function(number) => function(number, modifiers)?,
    };
    if modifiers.alt && !matches!(key, TerminalKey::Char(_)) && !alt_is_parameter {
        encoded.insert(0, 0x1b);
    }
    Ok(encoded)
}

fn encode_char(ch: char, modifiers: Modifiers) -> Vec<u8> {
    let mut bytes = if modifiers.control {
        let upper = ch.to_ascii_uppercase();
        match upper {
            '@'..='_' => vec![(upper as u8) & 0x1f],
            '?' => vec![0x7f],
            _ => {
                let mut storage = [0; 4];
                ch.encode_utf8(&mut storage).as_bytes().to_vec()
            }
        }
    } else {
        let mut storage = [0; 4];
        ch.encode_utf8(&mut storage).as_bytes().to_vec()
    };
    if modifiers.alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn modifier_parameter(modifiers: Modifiers) -> u8 {
    1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.control)
}

fn cursor(final_byte: u8, application: bool, modifiers: Modifiers) -> Vec<u8> {
    let parameter = modifier_parameter(modifiers);
    if parameter == 1 {
        vec![0x1b, if application { b'O' } else { b'[' }, final_byte]
    } else {
        format!("\x1b[1;{parameter}{}", char::from(final_byte)).into_bytes()
    }
}

fn tilde(code: u8, modifiers: Modifiers) -> Vec<u8> {
    let parameter = modifier_parameter(modifiers);
    if parameter == 1 {
        format!("\x1b[{code}~").into_bytes()
    } else {
        format!("\x1b[{code};{parameter}~").into_bytes()
    }
}

fn function(number: u8, modifiers: Modifiers) -> Result<Vec<u8>, InputError> {
    let parameter = modifier_parameter(modifiers);
    let ss3_final = match number {
        1 => Some(b'P'),
        2 => Some(b'Q'),
        3 => Some(b'R'),
        4 => Some(b'S'),
        5..=12 => None,
        _ => return Err(InputError::UnsupportedFunction(number)),
    };
    if let Some(final_byte) = ss3_final {
        return if parameter == 1 {
            Ok(vec![0x1b, b'O', final_byte])
        } else {
            Ok(format!("\x1b[1;{parameter}{}", char::from(final_byte)).into_bytes())
        };
    }
    let code = match number {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => unreachable!("F1-F4 and unsupported function keys returned above"),
    };
    Ok(tilde(code, modifiers))
}

#[must_use]
pub fn encode_focus(focused: bool, modes: TerminalModes) -> Option<Vec<u8>> {
    modes.focus_reporting.then(|| {
        if focused {
            b"\x1b[I".to_vec()
        } else {
            b"\x1b[O".to_vec()
        }
    })
}

#[must_use]
pub fn encode_alternate_scroll(lines: i32, modes: TerminalModes) -> Option<Vec<u8>> {
    if lines == 0
        || modes.mouse_protocol != MouseProtocol::None
        || !modes.alternate_screen
        || !modes.alternate_scroll
    {
        return None;
    }

    let key = if lines > 0 {
        TerminalKey::Up
    } else {
        TerminalKey::Down
    };
    let sequence = encode_key(key, Modifiers::default(), modes).ok()?;
    let count = usize::try_from(lines.unsigned_abs().min(12)).ok()?;
    Some(sequence.repeat(count))
}

#[allow(clippy::missing_errors_doc)]
pub fn encode_mouse(
    button: MouseButton,
    state: ButtonState,
    column: u16,
    line: u16,
    modifiers: Modifiers,
    modes: TerminalModes,
) -> Result<Option<Vec<u8>>, InputError> {
    if modes.mouse_protocol == MouseProtocol::None || modifiers.shift {
        return Ok(None);
    }
    let x = u32::from(column) + 1;
    let y = u32::from(line) + 1;
    let base = match (button, state) {
        (MouseButton::Left, ButtonState::Pressed | ButtonState::Motion) => 0,
        (MouseButton::Middle, ButtonState::Pressed | ButtonState::Motion) => 1,
        (MouseButton::Right, ButtonState::Pressed | ButtonState::Motion) => 2,
        (_, ButtonState::Released) => 3,
        (MouseButton::WheelUp, _) => 64,
        (MouseButton::WheelDown, _) => 65,
    };
    let code = base
        + 4 * u8::from(modifiers.shift)
        + 8 * u8::from(modifiers.alt)
        + 16 * u8::from(modifiers.control)
        + 32 * u8::from(state == ButtonState::Motion);
    match modes.mouse_encoding {
        MouseEncoding::Sgr => Ok(Some(
            format!(
                "\x1b[<{code};{x};{y}{}",
                if state == ButtonState::Released {
                    'm'
                } else {
                    'M'
                }
            )
            .into_bytes(),
        )),
        MouseEncoding::Legacy => {
            if x > 223 || y > 223 {
                return Err(InputError::MouseCoordinate);
            }
            Ok(Some(vec![
                0x1b,
                b'[',
                b'M',
                code + 32,
                u8::try_from(x).map_err(|_| InputError::MouseCoordinate)? + 32,
                u8::try_from(y).map_err(|_| InputError::MouseCoordinate)? + 32,
            ]))
        }
        MouseEncoding::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            for value in [u32::from(code) + 32, x + 32, y + 32] {
                out.extend(
                    char::from_u32(value)
                        .ok_or(InputError::MouseCoordinate)?
                        .to_string()
                        .bytes(),
                );
            }
            Ok(Some(out))
        }
    }
}

#[allow(clippy::missing_errors_doc)]
pub fn paste_transaction(text: &str, bracketed: bool) -> Result<Vec<u8>, InputError> {
    if text.contains('\0') {
        return Err(InputError::Nul);
    }
    let normalized = text.replace("\r\n", "\n").replace('\n', "\r");
    let wrapper = if bracketed {
        MAX_TRANSACTION_BYTES - MAX_PASTE_BODY_BYTES
    } else {
        0
    };
    if normalized.len() > MAX_TRANSACTION_BYTES - wrapper {
        return Err(InputError::Capacity);
    }
    let mut out = Vec::with_capacity(normalized.len() + wrapper);
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
    }
    out.extend_from_slice(normalized.as_bytes());
    if bracketed {
        out.extend_from_slice(b"\x1b[201~");
    }
    Ok(out)
}

#[allow(clippy::missing_errors_doc)]
pub fn commit_text(text: &str) -> Result<Vec<u8>, InputError> {
    if text.len() > MAX_COMMIT_BYTES || text.contains('\0') {
        return Err(InputError::Capacity);
    }
    Ok(text.as_bytes().to_vec())
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum InputError {
    #[error("only F1 through F12 are supported")]
    UnsupportedFunction(u8),
    #[error("mouse coordinate is outside the selected encoding range")]
    MouseCoordinate,
    #[error("input contains NUL")]
    Nul,
    #[error("input transaction exceeds its capacity")]
    Capacity,
    #[error("keyboard event has no protocol key code")]
    UnknownKey,
    #[error("associated text exceeds keyboard protocol limits")]
    AssociatedTextCapacity,
    #[error("associated text contains a control codepoint")]
    AssociatedTextControl,
    #[error("encoded keyboard event exceeds 256 bytes")]
    EncodedKeyCapacity,
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_gfx::{KeyIdentity, KeyLocation};
    fn modes() -> TerminalModes {
        TerminalModes::default()
    }

    fn keyboard_event(logical: LogicalKey, kind: KeyboardEventKind) -> TerminalKeyboardEvent {
        TerminalKeyboardEvent {
            identity: KeyIdentity {
                logical,
                location: KeyLocation::Standard,
                keypad: None,
                base_codepoint: match logical {
                    LogicalKey::Character(ch) => Some(ch),
                    _ => None,
                },
                shifted_codepoint: None,
            },
            text: match logical {
                LogicalKey::Character(ch) => Some(ch.to_string()),
                _ => None,
            },
            modifiers: Modifiers::default(),
            caps_lock: false,
            num_lock: false,
            kind,
            associated_text_allowed: true,
        }
    }

    fn kitty_modes(bits: u8) -> TerminalModes {
        TerminalModes {
            keyboard: super::super::KeyboardProtocolState {
                kitty: KittyKeyboardFlags::from_valid_bits(bits).unwrap(),
                modify_other_keys: ModifyOtherKeysLevel::Disabled,
            },
            ..TerminalModes::default()
        }
    }

    #[test]
    fn kitty_event_and_all_keys_truth_table_is_explicit() {
        for bits in [0, 2, 8, 10] {
            for kind in [
                KeyboardEventKind::Press,
                KeyboardEventKind::Repeat,
                KeyboardEventKind::Release,
            ] {
                let event = keyboard_event(LogicalKey::Character('a'), kind);
                let encoded = encode_keyboard_event(&event, kitty_modes(bits)).unwrap();
                match (bits, kind) {
                    (0 | 2, KeyboardEventKind::Press | KeyboardEventKind::Repeat) => {
                        assert_eq!(encoded, EncodedKey::TextFallback);
                    }
                    (8, KeyboardEventKind::Press | KeyboardEventKind::Repeat) => {
                        assert_eq!(encoded, EncodedKey::Bytes(b"\x1b[97;1u".to_vec()));
                    }
                    (10, KeyboardEventKind::Press) => {
                        assert_eq!(encoded, EncodedKey::Bytes(b"\x1b[97;1:1u".to_vec()));
                    }
                    (10, KeyboardEventKind::Repeat) => {
                        assert_eq!(encoded, EncodedKey::Bytes(b"\x1b[97;1:2u".to_vec()));
                    }
                    (10, KeyboardEventKind::Release) => {
                        assert_eq!(encoded, EncodedKey::Bytes(b"\x1b[97;1:3u".to_vec()));
                    }
                    (_, KeyboardEventKind::Release) => assert_eq!(
                        encoded,
                        EncodedKey::Ignored(IgnoreReason::ReleaseNotReported)
                    ),
                    _ => unreachable!(),
                }
            }
        }

        let arrow = keyboard_event(LogicalKey::ArrowUp, KeyboardEventKind::Repeat);
        assert_eq!(
            encode_keyboard_event(&arrow, kitty_modes(2)).unwrap(),
            EncodedKey::Bytes(b"\x1b[1;1:2A".to_vec())
        );
    }

    #[test]
    fn codex_flags_keep_legacy_functional_key_identities() {
        let cases = [
            (LogicalKey::ArrowLeft, b"\x1b[1;1:1D".as_slice()),
            (LogicalKey::ArrowRight, b"\x1b[1;1:1C".as_slice()),
            (LogicalKey::Home, b"\x1b[1;1:1H".as_slice()),
            (LogicalKey::Delete, b"\x1b[3;1:1~".as_slice()),
            (LogicalKey::PageDown, b"\x1b[6;1:1~".as_slice()),
            (LogicalKey::Function(1), b"\x1b[1;1:1P".as_slice()),
            (LogicalKey::Function(12), b"\x1b[24;1:1~".as_slice()),
        ];
        for (key, expected) in cases {
            let event = keyboard_event(key, KeyboardEventKind::Press);
            assert_eq!(
                encode_keyboard_event(&event, kitty_modes(7)).unwrap(),
                EncodedKey::Bytes(expected.to_vec())
            );
        }

        let release = keyboard_event(LogicalKey::ArrowLeft, KeyboardEventKind::Release);
        assert_eq!(
            encode_keyboard_event(&release, kitty_modes(7)).unwrap(),
            EncodedKey::Bytes(b"\x1b[1;1:3D".to_vec())
        );

        let all_keys = keyboard_event(LogicalKey::ArrowLeft, KeyboardEventKind::Press);
        assert_eq!(
            encode_keyboard_event(&all_keys, kitty_modes(8)).unwrap(),
            EncodedKey::Bytes(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn kitty_ignores_unidentified_system_keys() {
        let event = keyboard_event(
            LogicalKey::Unidentified(0x1008_ff4a),
            KeyboardEventKind::Press,
        );
        assert_eq!(
            encode_keyboard_event(&event, kitty_modes(8)).unwrap(),
            EncodedKey::Ignored(IgnoreReason::UnknownKey)
        );
    }

    #[test]
    fn kitty_alternate_associated_text_and_release_are_bounded() {
        let mut event = keyboard_event(LogicalKey::Character('A'), KeyboardEventKind::Press);
        event.identity.base_codepoint = Some('a');
        event.identity.shifted_codepoint = Some('A');
        event.modifiers.shift = true;
        event.text = Some("A".into());
        assert_eq!(
            encode_keyboard_event(&event, kitty_modes(4 | 8 | 16)).unwrap(),
            EncodedKey::Bytes(b"\x1b[97:65:97;2;65u".to_vec())
        );
        event.text = Some("x".repeat(MAX_ASSOCIATED_TEXT_SCALARS + 1));
        assert_eq!(
            encode_keyboard_event(&event, kitty_modes(8 | 16)),
            Err(InputError::AssociatedTextCapacity)
        );
    }

    #[test]
    fn kitty_precedes_mok_and_application_keypad_is_independent() {
        let mut event = keyboard_event(LogicalKey::Character('a'), KeyboardEventKind::Press);
        event.modifiers.control = true;
        let mut state = kitty_modes(1);
        state.keyboard.modify_other_keys = ModifyOtherKeysLevel::All;
        assert_eq!(
            encode_keyboard_event(&event, state).unwrap(),
            EncodedKey::Bytes(b"\x1b[97;5u".to_vec())
        );
        state.keyboard.kitty = KittyKeyboardFlags::default();
        assert_eq!(
            encode_keyboard_event(&event, state).unwrap(),
            EncodedKey::Bytes(b"\x1b[27;5;97~".to_vec())
        );

        let mut keypad = keyboard_event(LogicalKey::Enter, KeyboardEventKind::Press);
        keypad.identity.location = KeyLocation::Numpad;
        keypad.identity.keypad = Some(KeypadKey::Enter);
        assert_eq!(
            encode_keyboard_event(
                &keypad,
                TerminalModes {
                    application_keypad: true,
                    ..modes()
                }
            )
            .unwrap(),
            EncodedKey::Bytes(b"\x1bOM".to_vec())
        );
    }

    #[test]
    fn modify_other_keys_level_one_preserves_xterm_well_defined_aliases() {
        let modes = TerminalModes {
            keyboard: super::super::KeyboardProtocolState {
                kitty: KittyKeyboardFlags::default(),
                modify_other_keys: ModifyOtherKeysLevel::ExceptWellDefined,
            },
            ..TerminalModes::default()
        };
        let mut shifted = keyboard_event(LogicalKey::Character('A'), KeyboardEventKind::Press);
        shifted.identity.base_codepoint = Some('a');
        shifted.modifiers.shift = true;
        assert_eq!(
            encode_keyboard_event(&shifted, modes).unwrap(),
            EncodedKey::TextFallback
        );
        let cases = [
            (
                'a',
                'a',
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                b"\x01".as_slice(),
            ),
            (
                'a',
                'a',
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
                b"\x1b[27;3;97~".as_slice(),
            ),
            (
                '?',
                '/',
                Modifiers {
                    control: true,
                    shift: true,
                    ..Modifiers::default()
                },
                b"\x1b[27;6;63~".as_slice(),
            ),
            (
                '3',
                '3',
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                b"3".as_slice(),
            ),
            (
                '#',
                '3',
                Modifiers {
                    control: true,
                    shift: true,
                    ..Modifiers::default()
                },
                b"\x1b[27;6;35~".as_slice(),
            ),
        ];
        for (ch, base, modifiers, expected) in cases {
            let mut event = keyboard_event(LogicalKey::Character(ch), KeyboardEventKind::Press);
            event.identity.base_codepoint = Some(base);
            event.modifiers = modifiers;
            assert_eq!(
                encode_keyboard_event(&event, modes).unwrap(),
                EncodedKey::Bytes(expected.to_vec()),
                "character {ch:?}"
            );
        }
    }

    #[test]
    fn application_keypad_table_and_numeric_navigation_are_deterministic() {
        let application = TerminalModes {
            application_keypad: true,
            ..modes()
        };
        let table: [(KeypadKey, &[u8]); 18] = [
            (KeypadKey::Digit(0), b"\x1bOp"),
            (KeypadKey::Digit(1), b"\x1bOq"),
            (KeypadKey::Digit(2), b"\x1bOr"),
            (KeypadKey::Digit(3), b"\x1bOs"),
            (KeypadKey::Digit(4), b"\x1bOt"),
            (KeypadKey::Digit(5), b"\x1bOu"),
            (KeypadKey::Digit(6), b"\x1bOv"),
            (KeypadKey::Digit(7), b"\x1bOw"),
            (KeypadKey::Digit(8), b"\x1bOx"),
            (KeypadKey::Digit(9), b"\x1bOy"),
            (KeypadKey::Decimal, b"\x1bOn"),
            (KeypadKey::Divide, b"\x1bOo"),
            (KeypadKey::Multiply, b"\x1bOj"),
            (KeypadKey::Subtract, b"\x1bOm"),
            (KeypadKey::Add, b"\x1bOk"),
            (KeypadKey::Separator, b"\x1bOl"),
            (KeypadKey::Equal, b"\x1bOX"),
            (KeypadKey::Enter, b"\x1bOM"),
        ];
        for (keypad, expected) in table {
            let mut event = keyboard_event(LogicalKey::Unidentified(0), KeyboardEventKind::Press);
            event.identity.location = KeyLocation::Numpad;
            event.identity.keypad = Some(keypad);
            assert_eq!(
                encode_keyboard_event(&event, application).unwrap(),
                EncodedKey::Bytes(expected.to_vec()),
                "keypad {keypad:?}"
            );
        }

        let mut navigation = keyboard_event(LogicalKey::Home, KeyboardEventKind::Press);
        navigation.identity.location = KeyLocation::Numpad;
        navigation.identity.keypad = Some(KeypadKey::Digit(7));
        navigation.text = None;
        assert_eq!(
            encode_keyboard_event(&navigation, modes()).unwrap(),
            EncodedKey::Bytes(b"\x1b[H".to_vec())
        );
        let mut modified = navigation.clone();
        modified.identity.logical = LogicalKey::Unidentified(0);
        modified.identity.keypad = Some(KeypadKey::Add);
        modified.modifiers.control = true;
        assert_eq!(
            encode_keyboard_event(&modified, application).unwrap(),
            EncodedKey::Bytes(b"\x1b[1;5k".to_vec())
        );
    }
    #[test]
    fn keys_cover_control_application_and_modifiers() {
        assert_eq!(
            encode_key(
                TerminalKey::Char('c'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                modes()
            )
            .unwrap(),
            b"\x03"
        );
        assert_eq!(
            encode_key(
                TerminalKey::Up,
                Modifiers::default(),
                TerminalModes {
                    application_cursor: true,
                    ..modes()
                }
            )
            .unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(
                TerminalKey::Left,
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                modes()
            )
            .unwrap(),
            b"\x1b[1;5D"
        );
        assert_eq!(
            encode_key(
                TerminalKey::Left,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
                modes()
            )
            .unwrap(),
            b"\x1b[1;3D"
        );
        assert_eq!(
            encode_key(
                TerminalKey::Enter,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
                modes()
            )
            .unwrap(),
            b"\x1b\r"
        );
        assert_eq!(
            encode_key(
                TerminalKey::Char('"'),
                Modifiers {
                    shift: true,
                    control: true,
                    ..Modifiers::default()
                },
                modes()
            )
            .unwrap(),
            b"\""
        );
        assert_eq!(
            encode_key(
                TerminalKey::Char('%'),
                Modifiers {
                    shift: true,
                    control: true,
                    ..Modifiers::default()
                },
                modes()
            )
            .unwrap(),
            b"%"
        );
    }

    #[test]
    fn function_keys_match_xterm_256color() {
        let expected: [&[u8]; 12] = [
            b"\x1bOP",
            b"\x1bOQ",
            b"\x1bOR",
            b"\x1bOS",
            b"\x1b[15~",
            b"\x1b[17~",
            b"\x1b[18~",
            b"\x1b[19~",
            b"\x1b[20~",
            b"\x1b[21~",
            b"\x1b[23~",
            b"\x1b[24~",
        ];
        for (index, expected) in expected.into_iter().enumerate() {
            let number = u8::try_from(index + 1).unwrap();
            assert_eq!(
                encode_key(TerminalKey::Function(number), Modifiers::default(), modes()).unwrap(),
                expected,
                "F{number}"
            );
        }
    }

    #[test]
    fn modified_function_keys_use_xterm_modifier_parameters() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        let control = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_key(TerminalKey::Function(1), shift, modes()).unwrap(),
            b"\x1b[1;2P"
        );
        assert_eq!(
            encode_key(TerminalKey::Function(4), alt, modes()).unwrap(),
            b"\x1b[1;3S"
        );
        assert_eq!(
            encode_key(TerminalKey::Function(5), control, modes()).unwrap(),
            b"\x1b[15;5~"
        );
        assert_eq!(
            encode_key(TerminalKey::Function(0), Modifiers::default(), modes()),
            Err(InputError::UnsupportedFunction(0))
        );
        assert_eq!(
            encode_key(TerminalKey::Function(13), Modifiers::default(), modes()),
            Err(InputError::UnsupportedFunction(13))
        );
    }

    #[test]
    fn navigation_keys_match_normal_application_and_modified_xterm_forms() {
        let normal: [(TerminalKey, &[u8]); 10] = [
            (TerminalKey::Up, b"\x1b[A"),
            (TerminalKey::Down, b"\x1b[B"),
            (TerminalKey::Right, b"\x1b[C"),
            (TerminalKey::Left, b"\x1b[D"),
            (TerminalKey::Home, b"\x1b[H"),
            (TerminalKey::End, b"\x1b[F"),
            (TerminalKey::Insert, b"\x1b[2~"),
            (TerminalKey::Delete, b"\x1b[3~"),
            (TerminalKey::PageUp, b"\x1b[5~"),
            (TerminalKey::PageDown, b"\x1b[6~"),
        ];
        for (key, expected) in normal {
            assert_eq!(
                encode_key(key, Modifiers::default(), modes()).unwrap(),
                expected
            );
        }

        let application = TerminalModes {
            application_cursor: true,
            ..modes()
        };
        assert_eq!(
            encode_key(TerminalKey::Up, Modifiers::default(), application).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(TerminalKey::Home, Modifiers::default(), application).unwrap(),
            b"\x1bOH"
        );
        let shift_alt_control = Modifiers {
            shift: true,
            alt: true,
            control: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_key(TerminalKey::Right, shift_alt_control, application).unwrap(),
            b"\x1b[1;8C"
        );
        assert_eq!(
            encode_key(TerminalKey::PageDown, shift_alt_control, application).unwrap(),
            b"\x1b[6;8~"
        );
    }
    #[test]
    fn sgr_mouse_is_checked_and_shift_overrides() {
        let mouse = TerminalModes {
            mouse_protocol: MouseProtocol::AnyEvent,
            mouse_encoding: MouseEncoding::Sgr,
            ..modes()
        };
        assert_eq!(
            encode_mouse(
                MouseButton::Left,
                ButtonState::Pressed,
                2,
                3,
                Modifiers::default(),
                mouse
            )
            .unwrap()
            .unwrap(),
            b"\x1b[<0;3;4M"
        );
        assert!(
            encode_mouse(
                MouseButton::Left,
                ButtonState::Pressed,
                2,
                3,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                mouse
            )
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn bracketed_paste_is_one_normalized_transaction() {
        assert_eq!(
            paste_transaction("a\r\nb\n", true).unwrap(),
            b"\x1b[200~a\rb\r\x1b[201~"
        );
        assert_eq!(paste_transaction("bad\0", false), Err(InputError::Nul));
    }

    #[test]
    fn alternate_scroll_uses_bounded_cursor_sequences() {
        let alternate = TerminalModes {
            alternate_screen: true,
            alternate_scroll: true,
            application_cursor: true,
            ..modes()
        };
        assert_eq!(
            encode_alternate_scroll(3, alternate).unwrap(),
            b"\x1bOA\x1bOA\x1bOA"
        );
        assert_eq!(
            encode_alternate_scroll(-20, alternate).unwrap(),
            b"\x1bOB".repeat(12)
        );
        assert!(encode_alternate_scroll(3, modes()).is_none());
        assert!(
            encode_alternate_scroll(
                3,
                TerminalModes {
                    mouse_protocol: MouseProtocol::Normal,
                    ..alternate
                }
            )
            .is_none()
        );
    }
}

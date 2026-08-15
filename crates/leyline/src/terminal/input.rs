use super::{MouseEncoding, MouseProtocol, TerminalModes};

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
}

#[cfg(test)]
mod tests {
    use super::*;
    fn modes() -> TerminalModes {
        TerminalModes::default()
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

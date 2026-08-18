use leyline::terminal::{
    EncodedKey, GridSize, IgnoreReason, InputError, KeyboardEventKind, KittyKeyboardFlags,
    Modifiers, ModifyOtherKeysLevel, TerminalAction, TerminalCoreAdapter, TerminalKeyboardEvent,
    TerminalModes, encode_keyboard_event,
};
use leyline_gfx::{KeyIdentity, KeyLocation, LogicalKey};

const DISAMBIGUATE: u8 = 1;
const REPORT_EVENTS: u8 = 2;
const ALTERNATE_KEYS: u8 = 4;
const ALL_KEYS: u8 = 8;
const ASSOCIATED_TEXT: u8 = 16;

const LEGACY_FUNCTIONAL_CASES: &[(LogicalKey, &[u8], &[u8])] = &[
    (LogicalKey::ArrowUp, b"\x1b[A", b"\x1b[1;1:2A"),
    (LogicalKey::ArrowDown, b"\x1b[B", b"\x1b[1;1:2B"),
    (LogicalKey::ArrowRight, b"\x1b[C", b"\x1b[1;1:2C"),
    (LogicalKey::ArrowLeft, b"\x1b[D", b"\x1b[1;1:2D"),
    (LogicalKey::Home, b"\x1b[H", b"\x1b[1;1:2H"),
    (LogicalKey::End, b"\x1b[F", b"\x1b[1;1:2F"),
    (LogicalKey::Insert, b"\x1b[2~", b"\x1b[2;1:2~"),
    (LogicalKey::Delete, b"\x1b[3~", b"\x1b[3;1:2~"),
    (LogicalKey::PageUp, b"\x1b[5~", b"\x1b[5;1:2~"),
    (LogicalKey::PageDown, b"\x1b[6~", b"\x1b[6;1:2~"),
    (LogicalKey::Function(1), b"\x1bOP", b"\x1b[1;1:2P"),
    (LogicalKey::Function(2), b"\x1bOQ", b"\x1b[1;1:2Q"),
    (LogicalKey::Function(3), b"\x1bOR", b"\x1b[1;1:2R"),
    (LogicalKey::Function(4), b"\x1bOS", b"\x1b[1;1:2S"),
    (LogicalKey::Function(5), b"\x1b[15~", b"\x1b[15;1:2~"),
    (LogicalKey::Function(6), b"\x1b[17~", b"\x1b[17;1:2~"),
    (LogicalKey::Function(7), b"\x1b[18~", b"\x1b[18;1:2~"),
    (LogicalKey::Function(8), b"\x1b[19~", b"\x1b[19;1:2~"),
    (LogicalKey::Function(9), b"\x1b[20~", b"\x1b[20;1:2~"),
    (LogicalKey::Function(10), b"\x1b[21~", b"\x1b[21;1:2~"),
    (LogicalKey::Function(11), b"\x1b[23~", b"\x1b[23;1:2~"),
    (LogicalKey::Function(12), b"\x1b[24~", b"\x1b[24;1:2~"),
];

fn modes(flags: u8) -> TerminalModes {
    let mut modes = TerminalModes::default();
    modes.keyboard.kitty = KittyKeyboardFlags::from_valid_bits(flags).unwrap();
    modes
}

fn event(logical: LogicalKey, kind: KeyboardEventKind) -> TerminalKeyboardEvent {
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

fn encoded(flags: u8, event: &TerminalKeyboardEvent) -> EncodedKey {
    encode_keyboard_event(event, modes(flags)).unwrap()
}

fn kitty_text(codepoint: u32, flags: u8, kind: KeyboardEventKind, has_base: bool) -> Vec<u8> {
    let alternate = if has_base && flags & ALTERNATE_KEYS != 0 {
        format!("::{codepoint}")
    } else {
        String::new()
    };
    let mut output = format!("\x1b[{codepoint}{alternate};1");
    if flags & REPORT_EVENTS != 0 {
        let event_type = match kind {
            KeyboardEventKind::Press => 1,
            KeyboardEventKind::Repeat => 2,
            KeyboardEventKind::Release => 3,
        };
        output.push(':');
        output.push_str(&event_type.to_string());
    }
    if flags & ASSOCIATED_TEXT != 0 && kind != KeyboardEventKind::Release {
        output.push(';');
        output.push_str(&codepoint.to_string());
    }
    output.push('u');
    output.into_bytes()
}

#[test]
fn all_flag_combinations_route_plain_text_from_a_declarative_oracle() {
    for flags in 0..=KittyKeyboardFlags::VALID_BITS {
        for kind in [
            KeyboardEventKind::Press,
            KeyboardEventKind::Repeat,
            KeyboardEventKind::Release,
        ] {
            let actual = encoded(flags, &event(LogicalKey::Character('a'), kind));
            let expected = if flags & ALL_KEYS == 0 {
                if kind == KeyboardEventKind::Release {
                    EncodedKey::Ignored(IgnoreReason::ReleaseNotReported)
                } else {
                    EncodedKey::TextFallback
                }
            } else if kind == KeyboardEventKind::Release && flags & REPORT_EVENTS == 0 {
                EncodedKey::Ignored(IgnoreReason::ReleaseNotReported)
            } else {
                EncodedKey::Bytes(kitty_text(97, flags, kind, true))
            };
            assert_eq!(actual, expected, "flags={flags}, kind={kind:?}");
        }
    }
}

#[test]
fn all_flag_combinations_keep_cursor_keys_in_their_legacy_family() {
    for flags in 0..=KittyKeyboardFlags::VALID_BITS {
        for (kind, event_type) in [
            (KeyboardEventKind::Press, 1),
            (KeyboardEventKind::Repeat, 2),
            (KeyboardEventKind::Release, 3),
        ] {
            let actual = encoded(flags, &event(LogicalKey::ArrowLeft, kind));
            let expected = if flags & REPORT_EVENTS != 0 {
                EncodedKey::Bytes(format!("\x1b[1;1:{event_type}D").into_bytes())
            } else if kind == KeyboardEventKind::Release {
                EncodedKey::Ignored(IgnoreReason::ReleaseNotReported)
            } else {
                EncodedKey::Bytes(b"\x1b[D".to_vec())
            };
            assert_eq!(actual, expected, "flags={flags}, kind={kind:?}");
        }
    }
}

#[test]
fn complete_legacy_functional_table_preserves_terminfo_families() {
    for &(key, legacy, repeated) in LEGACY_FUNCTIONAL_CASES {
        assert_eq!(
            encoded(ALL_KEYS, &event(key, KeyboardEventKind::Press)),
            EncodedKey::Bytes(legacy.to_vec()),
            "legacy key={key:?}"
        );
        assert_eq!(
            encoded(REPORT_EVENTS, &event(key, KeyboardEventKind::Repeat)),
            EncodedKey::Bytes(repeated.to_vec()),
            "repeat key={key:?}"
        );
    }
}

#[test]
fn enter_tab_and_backspace_release_require_all_keys_and_event_types() {
    for flags in 0..=KittyKeyboardFlags::VALID_BITS {
        for (key, codepoint) in [
            (LogicalKey::Enter, 13),
            (LogicalKey::Tab, 9),
            (LogicalKey::Backspace, 127),
        ] {
            let release = event(key, KeyboardEventKind::Release);
            let actual = encoded(flags, &release);
            let expected = if flags & (ALL_KEYS | REPORT_EVENTS) == ALL_KEYS | REPORT_EVENTS {
                EncodedKey::Bytes(kitty_text(
                    codepoint,
                    flags,
                    KeyboardEventKind::Release,
                    false,
                ))
            } else {
                EncodedKey::Ignored(IgnoreReason::ReleaseNotReported)
            };
            assert_eq!(actual, expected, "flags={flags}, key={key:?}");
        }
    }
}

#[test]
fn shifted_character_uses_unshifted_primary_and_well_formed_alternates() {
    let mut shifted = event(LogicalKey::Character('A'), KeyboardEventKind::Press);
    shifted.identity.base_codepoint = Some('a');
    shifted.identity.shifted_codepoint = Some('A');
    shifted.modifiers.shift = true;
    shifted.text = Some("A".into());
    assert_eq!(
        encoded(ALL_KEYS, &shifted),
        EncodedKey::Bytes(b"\x1b[97;2u".to_vec())
    );
    assert_eq!(
        encoded(ALL_KEYS | ALTERNATE_KEYS, &shifted),
        EncodedKey::Bytes(b"\x1b[97:65:97;2u".to_vec())
    );

    let mut base_only = shifted.clone();
    base_only.identity.shifted_codepoint = None;
    assert_eq!(
        encoded(ALL_KEYS | ALTERNATE_KEYS, &base_only),
        EncodedKey::Bytes(b"\x1b[97::97;2u".to_vec())
    );

    let mut unshifted = shifted;
    unshifted.modifiers.shift = false;
    assert_eq!(
        encoded(ALL_KEYS | ALTERNATE_KEYS, &unshifted),
        EncodedKey::Bytes(b"\x1b[97::97;1u".to_vec())
    );
}

#[test]
fn omitted_parameters_use_the_default_for_each_protocol_operation() {
    let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 0).unwrap();
    core.advance(b"\x1b[=3;u").unwrap();
    assert_eq!(core.input_modes().keyboard.kitty.bits(), 3);

    core.advance(b"\x1b[>7u\x1b[<;u").unwrap();
    assert_eq!(core.input_modes().keyboard.kitty.bits(), 3);

    core.advance(b"\x1b[>4;2m\x1b[>4;m").unwrap();
    assert_eq!(
        core.input_modes().keyboard.modify_other_keys,
        ModifyOtherKeysLevel::Disabled
    );
}

#[test]
fn every_valid_flag_value_round_trips_through_set_and_query() {
    for flags in 0..=KittyKeyboardFlags::VALID_BITS {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 0).unwrap();
        core.advance(format!("\x1b[={flags};1u\x1b[?u").as_bytes())
            .unwrap();
        assert_eq!(core.input_modes().keyboard.kitty.bits(), flags);

        let mut actions = Vec::new();
        core.drain_actions(&mut actions);
        let replies: Vec<_> = actions
            .into_iter()
            .filter_map(|action| match action {
                TerminalAction::WriteToPty(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        assert_eq!(replies, [format!("\x1b[?{flags}u").into_bytes()]);
    }
}

#[test]
fn disambiguation_is_independent_from_plain_text_and_cursor_keys() {
    let plain = event(LogicalKey::Character('a'), KeyboardEventKind::Press);
    assert_eq!(encoded(DISAMBIGUATE, &plain), EncodedKey::TextFallback);

    let control = event(LogicalKey::Escape, KeyboardEventKind::Press);
    assert_eq!(
        encoded(DISAMBIGUATE, &control),
        EncodedKey::Bytes(b"\x1b[27;1u".to_vec())
    );

    let cursor = event(LogicalKey::ArrowRight, KeyboardEventKind::Press);
    assert_eq!(
        encoded(DISAMBIGUATE, &cursor),
        EncodedKey::Bytes(b"\x1b[C".to_vec())
    );
}

#[test]
fn non_legacy_function_keys_use_csi_u_even_with_zero_flags() {
    let f13 = event(LogicalKey::Function(13), KeyboardEventKind::Press);
    assert_eq!(
        encoded(0, &f13),
        EncodedKey::Bytes(b"\x1b[57376;1u".to_vec())
    );

    let release = event(LogicalKey::Function(35), KeyboardEventKind::Release);
    assert_eq!(
        encoded(0, &release),
        EncodedKey::Ignored(IgnoreReason::ReleaseNotReported)
    );
}

#[test]
fn associated_text_rejects_c0_and_c1_controls() {
    for text in ["a\nb", "a\u{7f}b", "a\u{85}b"] {
        let mut key = event(LogicalKey::Character('a'), KeyboardEventKind::Press);
        key.text = Some(text.into());
        assert_eq!(
            encode_keyboard_event(&key, modes(ALL_KEYS | ASSOCIATED_TEXT)),
            Err(InputError::AssociatedTextControl)
        );
    }
}

#[test]
fn vim_mok_disable_restores_control_bytes_at_the_encoder_boundary() {
    let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 0).unwrap();
    let mut control_c = event(LogicalKey::Character('c'), KeyboardEventKind::Press);
    control_c.modifiers.control = true;

    core.advance(b"\x1b[>4;2m").unwrap();
    assert_eq!(
        encode_keyboard_event(&control_c, core.input_modes()).unwrap(),
        EncodedKey::Bytes(b"\x1b[27;5;99~".to_vec())
    );

    core.advance(b"\x1b[>4;m").unwrap();
    assert_eq!(
        encode_keyboard_event(&control_c, core.input_modes()).unwrap(),
        EncodedKey::Bytes(vec![3])
    );
}

#[test]
fn protocol_state_is_invariant_across_every_two_chunk_split() {
    let sequence = b"\x1b[=3;1u\x1b[>7u\x1b[?1049h\x1b[=24;1u\x1b[?u\x1b[?1049l\x1b[<u\x1b[?u";
    for split in 0..=sequence.len() {
        let mut core = TerminalCoreAdapter::new(GridSize::new(8, 2).unwrap(), 0).unwrap();
        core.advance(&sequence[..split]).unwrap();
        core.advance(&sequence[split..]).unwrap();
        assert_eq!(core.input_modes().keyboard.kitty.bits(), 3, "split={split}");

        let mut actions = Vec::new();
        core.drain_actions(&mut actions);
        let replies: Vec<_> = actions
            .into_iter()
            .filter_map(|action| match action {
                TerminalAction::WriteToPty(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        assert_eq!(
            replies,
            [b"\x1b[?24u".to_vec(), b"\x1b[?3u".to_vec()],
            "split={split}"
        );
    }
}

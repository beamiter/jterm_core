use jterm_engine::input::{self, Key, Modifiers};

fn modifiers(bits: u8) -> Modifiers {
    Modifiers {
        shift: bits & 1 != 0,
        alt: bits & 2 != 0,
        ctrl: bits & 4 != 0,
        super_key: bits & 8 != 0,
    }
}

#[test]
fn functional_key_matrix_preserves_legacy_and_modified_bytes() {
    let cursors = [
        (Key::ArrowUp, 'A'),
        (Key::ArrowDown, 'B'),
        (Key::ArrowRight, 'C'),
        (Key::ArrowLeft, 'D'),
        (Key::Home, 'H'),
        (Key::End, 'F'),
    ];
    let tilde = [
        (Key::Insert, 2),
        (Key::Delete, 3),
        (Key::PageUp, 5),
        (Key::PageDown, 6),
        (Key::F5, 15),
        (Key::F6, 17),
        (Key::F7, 18),
        (Key::F8, 19),
        (Key::F9, 20),
        (Key::F10, 21),
        (Key::F11, 23),
        (Key::F12, 24),
    ];
    for bits in 0..16 {
        let mods = modifiers(bits);
        assert_eq!(input::kitty_modifier_value(mods), bits + 1);
        for application in [false, true] {
            for (key, suffix) in cursors {
                let expected = if bits != 0 {
                    format!("\x1b[1;{}{suffix}", bits + 1)
                } else if application {
                    format!("\x1bO{suffix}")
                } else {
                    format!("\x1b[{suffix}")
                };
                assert_eq!(
                    input::legacy_function_key_sequence(key, mods, application).as_deref(),
                    Some(expected.as_str())
                );
            }
            for (key, code) in tilde {
                let expected = if bits != 0 {
                    format!("\x1b[{code};{}~", bits + 1)
                } else {
                    format!("\x1b[{code}~")
                };
                assert_eq!(
                    input::legacy_function_key_sequence(key, mods, application).as_deref(),
                    Some(expected.as_str())
                );
            }
            for (key, suffix) in [
                (Key::F1, 'P'),
                (Key::F2, 'Q'),
                (Key::F3, 'R'),
                (Key::F4, 'S'),
            ] {
                let expected = if bits != 0 {
                    format!("\x1b[1;{}{suffix}", bits + 1)
                } else {
                    format!("\x1bO{suffix}")
                };
                assert_eq!(
                    input::legacy_function_key_sequence(key, mods, application).as_deref(),
                    Some(expected.as_str())
                );
            }
        }
    }
}

#[test]
fn kitty_flags_do_not_expand_the_hosts_negotiated_repertoire() {
    for flags in 0..32 {
        for bits in 0..16 {
            let expected = if flags & 8 != 0 || (flags & 1 != 0 && bits & 14 != 0) {
                Some(format!("\x1b[97;{}u", bits + 1))
            } else {
                None
            };
            assert_eq!(
                input::kitty_encode_key_event(Key::Character('a'), modifiers(bits), flags),
                expected
            );
            // GUI-level unknowns and unsupported logical Unicode must not acquire new wire behavior.
            for key in [Key::Unidentified, Key::Enter, Key::Character('é')] {
                assert_eq!(
                    input::kitty_encode_key_event(key, modifiers(bits), flags),
                    None
                );
            }
        }
    }
}

#[test]
fn modify_other_keys_level_format_and_modifier_matrix() {
    for level in 0..4 {
        for format in 0..3 {
            for all in [false, true] {
                for bits in 0..16 {
                    let active = if all {
                        bits != 0
                    } else {
                        match level {
                            0 => false,
                            1 => bits & 10 != 0,
                            _ => bits != 0,
                        }
                    };
                    let code = if bits & 1 != 0 { 65 } else { 97 };
                    let expected = active.then(|| {
                        if format == 1 || all {
                            format!("\x1b[{code};{}u", bits + 1)
                        } else {
                            format!("\x1b[27;{};{code}~", bits + 1)
                        }
                    });
                    assert_eq!(
                        input::xterm_encode_modify_other_keys(
                            Key::Character('a'),
                            modifiers(bits),
                            level,
                            format,
                            all,
                            None
                        ),
                        expected
                    );
                }
            }
        }
    }
}

#[test]
fn os_text_wins_over_layout_guess_and_unicode_is_preserved() {
    let shift = modifiers(1);
    assert_eq!(
        input::xterm_encode_modify_other_keys(Key::Character('1'), shift, 2, 0, false, Some('!')),
        Some("\x1b[27;2;33~".into())
    );
    assert_eq!(
        input::xterm_encode_modify_other_keys(Key::Unidentified, shift, 2, 1, false, Some('é')),
        Some("\x1b[233;2u".into())
    );
    assert_eq!(
        input::xterm_encode_modify_other_keys(Key::Character('1'), shift, 2, 0, false, None),
        Some("\x1b[27;2;49~".into())
    );
}

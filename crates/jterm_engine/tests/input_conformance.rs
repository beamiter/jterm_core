//! Independent literal wire fixtures; scope: Ember-compatible shared primitives.
use jterm_engine::{
    input::{self, Key, Modifiers},
    mode_mutation::{apply_ember_mode, ModeNamespace},
    parser::{Event, Parser, ParserLimits, ParserProfile, PendingKind},
    protocol_state::ProtocolState,
    screen::ScreenState,
    semantics::{dispatch_frost_legacy_csi, dispatch_local, Dispatch, Effect},
};

#[test]
fn frost_key_format_commands_preserve_wrap_and_cursor() {
    let mut screen = ScreenState::new(2, 4);
    let mut protocol = ProtocolState::default();
    screen.cursor_col = 3;
    screen.pending_wrap = true;
    for (raw, params, expected) in [
        (b"4;1", [4, 1], 1),
        (b"9;0", [9, 0], 1),
        (b"4;0", [4, 0], 0),
    ] {
        assert_eq!(
            dispatch_frost_legacy_csi(
                &mut screen,
                &mut protocol,
                raw,
                &params,
                b'f',
                Some(b'>'),
                b""
            ),
            Dispatch::Handled(Default::default())
        );
        assert!(screen.pending_wrap);
        assert_eq!((screen.cursor_row, screen.cursor_col), (0, 3));
        assert_eq!(protocol.xterm_format_other_keys(), expected);
    }
}

#[test]
fn frost_modify_other_keys_query_reports_effective_level_and_preserves_rendition() {
    let mut screen = ScreenState::new(2, 8);
    screen.current_fg = jterm_engine::Color::Red;
    screen.current_flags.set_bold(true);
    let rendition = screen.current_flags;
    let mut protocol = ProtocolState::default();
    for (level, expected) in [
        (0, b"\x1b[>4;0m".as_slice()),
        (1, b"\x1b[>4;1m"),
        (2, b"\x1b[>4;2m"),
        (65535, b"\x1b[>4;2m"),
    ] {
        protocol.set_xterm_modify_other_keys(level);
        for raw in [b"4".as_slice(), b"04", b"0004"] {
            assert_eq!(
                dispatch_frost_legacy_csi(
                    &mut screen,
                    &mut protocol,
                    raw,
                    &[4],
                    b'm',
                    Some(b'?'),
                    b""
                ),
                Dispatch::Handled([Effect::Reply(expected.to_vec())].into_iter().collect())
            );
        }
        assert_eq!(screen.current_fg, jterm_engine::Color::Red);
        assert_eq!(screen.current_flags, rendition);
    }
}

struct Session {
    parser: Parser,
    screen: ScreenState,
    protocol: ProtocolState,
}
impl Session {
    fn new() -> Self {
        Self {
            parser: Parser::with_bounded_profile(
                ParserLimits {
                    pending_bytes: 128,
                    kitty_recovery_bytes: 32,
                },
                ParserProfile::EmberV1,
            )
            .unwrap(),
            screen: ScreenState::new(6, 12),
            protocol: ProtocolState::default(),
        }
    }
    fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.parser.begin_feed();
        let mut at = 0;
        let mut replies = vec![];
        for _ in 0..bytes.len() * 4 + 32 {
            let step = self.parser.next(&bytes[at..]);
            assert!(step.consumed <= bytes.len() - at);
            at += step.consumed;
            match step.event {
                Some(Event::Boundary) => (),
                Some(Event::Csi {
                    ref params,
                    private_prefix: Some(b'?'),
                    ref intermediates,
                    final_byte,
                    ..
                }) if params.as_slice() == [1]
                    && intermediates.is_empty()
                    && matches!(final_byte, b'h' | b'l') =>
                {
                    // Explicit narrow host admission: whole CSI mode routing is not shared.
                    assert!(apply_ember_mode(
                        &mut self.screen,
                        &mut self.protocol,
                        ModeNamespace::DecPrivate,
                        1,
                        final_byte == b'h'
                    ));
                }
                Some(event) => {
                    let Dispatch::Handled(effects) =
                        dispatch_local(&mut self.screen, &mut self.protocol, &event)
                    else {
                        panic!("host-owned event in primitive-only fixture: {event:?}")
                    };
                    for effect in effects {
                        match effect {
                            Effect::Reply(reply) => replies.push(reply),
                            // Existing dispatch emits reposition metadata for CSI f.
                            Effect::CursorReposition { .. } => (),
                            other => panic!("unexpected effect {other:?}"),
                        }
                    }
                }
                None => {
                    assert_eq!(at, bytes.len());
                    return replies;
                }
            }
        }
        panic!("parser failed to drain");
    }
    fn command(&mut self, bytes: &[u8], chunk: usize, expected_reply: &[&[u8]]) {
        let mut replies = vec![];
        for part in bytes.chunks(chunk) {
            replies.extend(self.feed(part));
        }
        assert_eq!(replies, expected_reply);
        assert_eq!(self.parser.pending_kind(), PendingKind::None);
        assert_eq!(self.parser.rejected_count(), 0);
    }
    fn cursor(&self, mods: Modifiers) -> String {
        input::legacy_function_key_sequence(Key::ArrowUp, mods, self.protocol.modes().contains(&1))
            .unwrap()
            .into_owned()
    }
    fn kitty(&self, mods: Modifiers) -> Option<String> {
        input::kitty_encode_key_event(
            Key::Character('a'),
            mods,
            self.protocol.keyboard_enhancement_flags(),
        )
    }
    fn xterm(&self, mods: Modifiers, text: char) -> Option<String> {
        input::xterm_encode_modify_other_keys(
            Key::Unidentified,
            mods,
            self.protocol.xterm_modify_other_keys(),
            self.protocol.xterm_format_other_keys(),
            false,
            Some(text),
        )
    }
}
fn ctrl() -> Modifiers {
    Modifiers {
        ctrl: true,
        ..Modifiers::default()
    }
}
fn shift() -> Modifiers {
    Modifiers {
        shift: true,
        ..Modifiers::default()
    }
}
fn alt() -> Modifiers {
    Modifiers {
        alt: true,
        ..Modifiers::default()
    }
}

#[test]
fn decckm_negotiation_changes_plain_keys_but_not_modified_keys() {
    for chunk in 1..=32 {
        let mut s = Session::new();
        assert_eq!(s.cursor(Modifiers::default()), "\x1b[A");
        s.command(b"\x1b[?1h\x1b[?1$p", chunk, &[b"\x1b[?1;1$y"]);
        assert_eq!(s.cursor(Modifiers::default()), "\x1bOA");
        assert_eq!(s.cursor(ctrl()), "\x1b[1;5A");
        s.command(b"\x1b[?1l\x1b[?1$p", chunk, &[b"\x1b[?1;2$y"]);
        assert_eq!(s.cursor(Modifiers::default()), "\x1b[A");
        assert_eq!(s.cursor(ctrl()), "\x1b[1;5A");
        // CAN cancels the attempted enable; it must not leak into later input.
        s.command(b"\x1b[?1\x18\x1b[?1$p", chunk, &[b"\x1b[?1;2$y"]);
        assert_eq!(s.cursor(Modifiers::default()), "\x1b[A");
    }
}

#[test]
fn kitty_push_set_clear_pop_and_query_drive_literal_wire_bytes() {
    for chunk in 1..=32 {
        let mut s = Session::new();
        assert_eq!(s.kitty(ctrl()), None);
        s.command(b"\x1b[>1u\x1b[?u", chunk, &[b"\x1b[?1u"]);
        assert_eq!(s.kitty(ctrl()).as_deref(), Some("\x1b[97;5u"));
        assert_eq!(s.kitty(shift()), None);
        s.command(b"\x1b[=8;2u\x1b[?u", chunk, &[b"\x1b[?9u"]);
        assert_eq!(s.kitty(Modifiers::default()).as_deref(), Some("\x1b[97;1u"));
        s.command(b"\x1b[>0u\x1b[?u", chunk, &[b"\x1b[?0u"]);
        assert_eq!(s.kitty(ctrl()), None);
        s.command(b"\x1b[<u\x1b[?u", chunk, &[b"\x1b[?9u"]);
        s.command(b"\x1b[=8;3u\x1b[?u", chunk, &[b"\x1b[?1u"]);
        assert_eq!(s.kitty(Modifiers::default()), None);
        assert_eq!(s.kitty(alt()).as_deref(), Some("\x1b[97;3u"));
        s.command(b"\x1b[<u\x1b[?u", chunk, &[b"\x1b[?0u"]);
        assert_eq!(s.kitty(ctrl()), None);
        // Underflow stays disabled; a canceled push creates no stack entry.
        s.command(b"\x1b[>8\x18\x1b[?u", chunk, &[b"\x1b[?0u"]);
        assert!(s.protocol.keyboard_enhancement_stack().is_empty());
        assert_eq!(s.kitty(ctrl()), None);
        s.command(b"\x1b[<9u\x1b[?u", chunk, &[b"\x1b[?0u"]);
        assert!(s.protocol.keyboard_enhancement_stack().is_empty());
        assert_eq!(s.kitty(ctrl()), None);
    }
}

#[test]
fn modify_other_keys_negotiation_preserves_host_supplied_unicode() {
    for chunk in 1..=32 {
        let mut s = Session::new();
        assert_eq!(s.xterm(alt(), '界'), None);
        s.command(b"\x1b[>4;1m", chunk, &[]);
        assert_eq!(s.xterm(alt(), '界').as_deref(), Some("\x1b[27;3;30028~"));
        assert_eq!(s.xterm(shift(), '!'), None);
        s.command(b"\x1b[>4;2m", chunk, &[]);
        assert_eq!(s.xterm(shift(), '!').as_deref(), Some("\x1b[27;2;33~"));
        s.command(b"\x1b[>4;1f", chunk, &[]);
        assert_eq!(s.xterm(shift(), 'é').as_deref(), Some("\x1b[233;2u"));
        s.command(b"\x1b[>4;0m", chunk, &[]);
        assert_eq!(s.xterm(alt(), '界'), None);
        s.command(b"\x1b[>4;2\x18", chunk, &[]);
        assert_eq!(s.xterm(alt(), '界'), None);
    }
}

//! Bounded XTGETTCAP color capabilities shared by all terminal profiles.
//!
//! Replies use the shared color model and keep the order of admitted names.

use crate::{
    input::{legacy_function_key_sequence, Key, Modifiers},
    protocol_state::ProtocolState,
};
use std::borrow::Cow;

const MAX_QUERY_BYTES: usize = 1024;
const MAX_NAMES: usize = 32;
const MAX_HEX_NAME_BYTES: usize = 64;
const MAX_REPLY_BYTES: usize = 4096;
pub(crate) const NEGATIVE_TERMCAP_REPLY: &[u8] = b"\x1bP0+r\x1b\\";

fn color_value(hex_name: &[u8]) -> Option<&'static [u8]> {
    if hex_name.eq_ignore_ascii_case(b"436F") || hex_name.eq_ignore_ascii_case(b"636F6C6F7273") {
        Some(b"323536") // Co or colors, 256 indexed colors.
    } else if hex_name == b"524742" {
        Some(b"38") // RGB, 8 bits per channel.
    } else {
        None
    }
}

// XTerm's unmodified termcap and terminfo names. Host shortcut, keypad,
// text, Backspace and enhanced-key profiles are deliberately not advertised.
fn keyboard_key(hex_name: &[u8], protocol: &ProtocolState) -> Option<Key> {
    if protocol.keyboard_enhancement_flags() & !1 != 0 {
        return None;
    }
    const KEYS: &[(&[u8], Key)] = &[
        (b"6B75", Key::ArrowUp),          // ku
        (b"6B63757531", Key::ArrowUp),    // kcuu1
        (b"6B64", Key::ArrowDown),        // kd
        (b"6B63756431", Key::ArrowDown),  // kcud1
        (b"6B72", Key::ArrowRight),       // kr
        (b"6B63756631", Key::ArrowRight), // kcuf1
        (b"6B6C", Key::ArrowLeft),        // kl
        (b"6B63756231", Key::ArrowLeft),  // kcub1
        (b"6B68", Key::Home),             // kh
        (b"6B686F6D65", Key::Home),       // khome
        (b"4037", Key::End),              // @7
        (b"6B656E64", Key::End),          // kend
        (b"6B49", Key::Insert),           // kI
        (b"6B69636831", Key::Insert),     // kich1
        (b"6B44", Key::Delete),           // kD
        (b"6B64636831", Key::Delete),     // kdch1
        (b"6B50", Key::PageUp),           // kP
        (b"6B7070", Key::PageUp),         // kpp
        (b"6B4E", Key::PageDown),         // kN
        (b"6B6E70", Key::PageDown),       // knp
        (b"6B31", Key::F1),               // k1
        (b"6B6631", Key::F1),             // kf1
        (b"6B32", Key::F2),               // k2
        (b"6B6632", Key::F2),             // kf2
        (b"6B33", Key::F3),               // k3
        (b"6B6633", Key::F3),             // kf3
        (b"6B34", Key::F4),               // k4
        (b"6B6634", Key::F4),             // kf4
        (b"6B35", Key::F5),               // k5
        (b"6B6635", Key::F5),             // kf5
        (b"6B36", Key::F6),               // k6
        (b"6B6636", Key::F6),             // kf6
        (b"6B37", Key::F7),               // k7
        (b"6B6637", Key::F7),             // kf7
        (b"6B38", Key::F8),               // k8
        (b"6B6638", Key::F8),             // kf8
        (b"6B39", Key::F9),               // k9
        (b"6B6639", Key::F9),             // kf9
        (b"6B3B", Key::F10),              // k;
        (b"6B663130", Key::F10),          // kf10
        (b"4631", Key::F11),              // F1
        (b"6B663131", Key::F11),          // kf11
        (b"4632", Key::F12),              // F2
        (b"6B663132", Key::F12),          // kf12
    ];
    KEYS.iter()
        .find_map(|&(name, key)| hex_name.eq_ignore_ascii_case(name).then_some(key))
}
fn append_hex(output: &mut Vec<u8>, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in bytes {
        output.push(HEX[usize::from(byte >> 4)]);
        output.push(HEX[usize::from(byte & 15)]);
    }
}

/// Match the complete DCS payload `+q<hex-names>` and produce one bounded
/// 7-bit DCS response.
/// Unknown or malformed items stop the list without reflection. If none were
/// admitted, the historical fixed negative reply is borrowed without allocating.
pub(crate) fn termcap_query(
    payload: &[u8],
    protocol: &ProtocolState,
) -> Option<Cow<'static, [u8]>> {
    let names = payload.strip_prefix(b"+q")?;
    if names.len() > MAX_QUERY_BYTES {
        return Some(Cow::Borrowed(NEGATIVE_TERMCAP_REPLY));
    }
    for (index, name) in names.split(|&byte| byte == b';').enumerate() {
        if index >= MAX_NAMES || name.len() > MAX_HEX_NAME_BYTES {
            return Some(Cow::Borrowed(NEGATIVE_TERMCAP_REPLY));
        }
    }
    let mut reply = Vec::new();
    for name in names.split(|&byte| byte == b';') {
        let color = color_value(name);
        let key = keyboard_key(name, protocol);
        if color.is_none() && key.is_none() {
            break;
        }
        if reply.is_empty() {
            reply.reserve(payload.len() + MAX_NAMES * 8 + 8);
            reply.extend_from_slice(b"\x1bP1+r");
        } else {
            reply.push(b';');
        }
        reply.extend_from_slice(name);
        reply.push(b'=');
        if let Some(value) = color {
            reply.extend_from_slice(value);
        } else if let Some(key) = key {
            let sequence = legacy_function_key_sequence(
                key,
                Modifiers::default(),
                protocol.modes().contains(&1),
            )
            .expect("admitted functional key");
            append_hex(&mut reply, sequence.as_bytes());
        }
    }
    if reply.is_empty() {
        return Some(Cow::Borrowed(NEGATIVE_TERMCAP_REPLY));
    }
    reply.extend_from_slice(b"\x1b\\");
    debug_assert!(reply.len() <= MAX_REPLY_BYTES);
    Some(Cow::Owned(reply))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reply(payload: &[u8]) -> Vec<u8> {
        termcap_query(payload, &ProtocolState::default())
            .unwrap()
            .into_owned()
    }
    #[test]
    fn truthful_colors_preserve_name_hex_case_and_wire_order() {
        assert_eq!(reply(b"+q436f"), b"\x1bP1+r436f=323536\x1b\\");
        assert_eq!(
            reply(b"+q636F6c6F7273;524742;436F;436f"),
            b"\x1bP1+r636F6c6F7273=323536;524742=38;436F=323536;436f=323536\x1b\\"
        );
        // Names themselves are case sensitive, irrespective of hex spelling.
        assert_eq!(reply(b"+q636f"), NEGATIVE_TERMCAP_REPLY);
        assert_eq!(reply(b"+q5463"), NEGATIVE_TERMCAP_REPLY); // Tc is not advertised.
        assert_eq!(reply(b"+q544e"), NEGATIVE_TERMCAP_REPLY); // No invented terminal name.
    }
    #[test]
    fn unknown_and_malformed_names_stop_after_the_admitted_prefix() {
        assert_eq!(reply(b"+q7878;436f"), NEGATIVE_TERMCAP_REPLY);
        assert_eq!(reply(b"+q436f;7878;524742"), b"\x1bP1+r436f=323536\x1b\\");
        assert_eq!(reply(b"+q436f;zz;524742"), b"\x1bP1+r436f=323536\x1b\\");
        assert_eq!(reply(b"+q436f;"), b"\x1bP1+r436f=323536\x1b\\");
        for invalid in [
            b"+q".as_slice(),
            b"+q;436f",
            b"+q4",
            b"+qzz",
            b"+q0000",
            b"+q436f=323536",
        ] {
            assert_eq!(reply(invalid), NEGATIVE_TERMCAP_REPLY, "{invalid:?}");
            assert!(matches!(
                termcap_query(invalid, &ProtocolState::default()),
                Some(Cow::Borrowed(_))
            ));
        }
        for unrelated in [
            b"1+q436f".as_slice(),
            b"?+q436f",
            b"+Q436f",
            b"+p436f",
            b"+q 436f",
        ] {
            if unrelated.starts_with(b"+q") {
                assert_eq!(reply(unrelated), NEGATIVE_TERMCAP_REPLY);
            } else {
                assert!(termcap_query(unrelated, &ProtocolState::default()).is_none());
            }
        }
    }
    #[test]
    fn resource_limits_produce_one_fixed_negative_reply() {
        let mut count_boundary = b"+q".to_vec();
        count_boundary.extend(
            std::iter::repeat_n(b"436f".as_slice(), MAX_NAMES)
                .collect::<Vec<_>>()
                .join(&b';'),
        );
        assert!(reply(&count_boundary).len() < MAX_REPLY_BYTES);
        count_boundary.extend_from_slice(b";436f");
        assert_eq!(reply(&count_boundary), NEGATIVE_TERMCAP_REPLY);
        let mut long_name = b"+q".to_vec();
        long_name.extend(std::iter::repeat_n(b'4', MAX_HEX_NAME_BYTES + 2));
        assert_eq!(reply(&long_name), NEGATIVE_TERMCAP_REPLY);
        let mut long_query = b"+q".to_vec();
        long_query.extend(std::iter::repeat_n(b'4', MAX_QUERY_BYTES + 1));
        assert_eq!(reply(&long_query), NEGATIVE_TERMCAP_REPLY);
    }
    #[test]
    fn keyboard_wire_tracks_decckm_and_keeps_function_and_edit_keys_independent() {
        let mut protocol = ProtocolState::default();
        assert_eq!(
            termcap_query(b"+q6B75;6B68;6B6631;6B44", &protocol)
                .unwrap()
                .as_ref(),
            b"\x1bP1+r6B75=1B5B41;6B68=1B5B48;6B6631=1B4F50;6B44=1B5B337E\x1b\\"
        );
        protocol.modes_mut().insert(1);
        protocol.modes_mut().insert(66);
        protocol.set_xterm_modify_other_keys(2);
        protocol.set_xterm_format_other_keys(1);
        let before = format!("{protocol:?}");
        assert_eq!(
            termcap_query(b"+q6B63757531;6B686F6D65;6B6631;6B44", &protocol)
                .unwrap()
                .as_ref(),
            b"\x1bP1+r6B63757531=1B4F41;6B686F6D65=1B4F48;6B6631=1B4F50;6B44=1B5B337E\x1b\\"
        );
        assert_eq!(format!("{protocol:?}"), before);
        protocol.modes_mut().remove(&1);
        assert_eq!(
            termcap_query(b"+q6B75", &protocol).unwrap().as_ref(),
            b"\x1bP1+r6B75=1B5B41\x1b\\"
        );
    }
    #[test]
    fn non_common_enhanced_profiles_decline_key_caps_without_hiding_color_caps() {
        let mut protocol = ProtocolState::default();
        protocol.set_keyboard_enhancement_flags(1, 1);
        assert_eq!(
            termcap_query(b"+q6B75", &protocol).unwrap().as_ref(),
            b"\x1bP1+r6B75=1B5B41\x1b\\"
        );
        for flags in [2, 4, 8, 16, 31, u16::MAX] {
            protocol.set_keyboard_enhancement_flags(flags, 1);
            assert!(matches!(
                termcap_query(b"+q6B75;436F", &protocol),
                Some(Cow::Borrowed(NEGATIVE_TERMCAP_REPLY))
            ));
            assert_eq!(
                termcap_query(b"+q436F;6B75;524742", &protocol)
                    .unwrap()
                    .as_ref(),
                b"\x1bP1+r436F=323536\x1b\\"
            );
            assert_eq!(
                termcap_query(b"+q524742", &protocol).unwrap().as_ref(),
                b"\x1bP1+r524742=38\x1b\\"
            );
        }
    }
}

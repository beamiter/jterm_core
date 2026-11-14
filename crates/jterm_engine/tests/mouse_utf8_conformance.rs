//! Independent wire expectations for xterm extended UTF-8 mouse mode (1005).
//! Source: https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h3-Extended-coordinates
//! The specification encodes Cb, Cx and Cy as UTF-8, not only coordinates.
use jterm_engine::protocol_state::TerminalModes;

#[test]
fn extended_buttons_use_utf8_for_all_three_fields() {
    let mut modes = TerminalModes::default();
    modes.insert(1000);
    modes.insert(1005);
    // Handwritten bytes at ASCII / multibyte and previous saturation boundaries.
    for (button, expected) in [
        (0, b"\x1b[M !!".as_slice()),
        (64, b"\x1b[M`!!"),
        (95, b"\x1b[M\x7f!!"),
        (96, b"\x1b[M\xc2\x80!!"),
        (128, b"\x1b[M\xc2\xa0!!"),
        (223, b"\x1b[M\xc3\xbf!!"),
        (224, b"\x1b[M\xc4\x80!!"),
        (255, b"\x1b[M\xc4\x9f!!"),
    ] {
        assert_eq!(modes.get_mouse_report(button, 0, 0).unwrap(), expected);
    }
    // All u8 inputs remain distinct; coordinates keep the existing 2015 cap.
    for button in 0..=u8::MAX {
        let report = modes.get_mouse_report(button, 94, usize::MAX).unwrap();
        let text = std::str::from_utf8(&report).expect("1005 must be valid UTF-8");
        let fields: Vec<u32> = text[3..].chars().map(u32::from).collect();
        assert_eq!(fields, [u32::from(button) + 32, 127, 2047]);
    }
}

#[test]
fn utf8_mode_does_not_change_other_encoding_precedence_or_disabled_reporting() {
    let mut modes = TerminalModes::default();
    modes.insert(1005);
    assert_eq!(modes.get_mouse_report(128, 0, 0), None);
    modes.insert(1000);
    modes.insert(1015);
    assert_eq!(modes.get_mouse_report(128, 0, 0).unwrap(), b"\x1b[160;1;1M");
    modes.insert(1006);
    assert_eq!(
        modes.get_mouse_report(128, 0, 0).unwrap(),
        b"\x1b[<128;1;1M"
    );
    modes.remove(&1005);
    modes.remove(&1015);
    modes.remove(&1006);
    assert_eq!(modes.get_mouse_report(128, 0, 0).unwrap(), b"\x1b[M\xa0!!");
}

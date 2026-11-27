use jterm_engine::{
    mouse::{MouseEncoding, MouseInput, MousePosition},
    native_window_ops::NativeWindowPixels,
    terminal::Terminal,
};

#[test]
fn geometry_changes_retire_pointer_generation_only_after_success() {
    for reflow in [false, true] {
        let mut terminal = Terminal::new(3, 12).unwrap();
        if reflow {
            terminal.enable_primary_reflow();
        }
        terminal.feed(b"\x1b[?1002;1006h").unwrap();
        let initial = terminal.mouse_report_generation();
        terminal.resize(3, 12).unwrap();
        assert_eq!(terminal.mouse_report_generation(), initial);
        assert!(terminal
            .resize_with(4, 16, || Err("ioctl refused".into()))
            .is_err());
        assert_eq!(terminal.mouse_report_generation(), initial);
        terminal.resize(4, 16).unwrap();
        assert_eq!(terminal.mouse_report_generation(), initial + 1);
        terminal.resize(3, 12).unwrap();
        assert_eq!(terminal.mouse_report_generation(), initial + 2);
        assert!(terminal.mouse_report_profile().is_some());
    }
}

#[test]
fn pointer_generation_records_hidden_profile_transitions_without_paint() {
    let packet = b"\x1b[?1000h\x1b[?1000l\x1b[?1000h";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(3, 12).unwrap();
        let before = terminal.frame().unwrap();
        let generation = terminal.mouse_report_generation();
        terminal.feed(&packet[..split]).unwrap();
        terminal.feed(&packet[split..]).unwrap();
        assert_eq!(terminal.mouse_report_generation(), generation + 3);
        assert!(terminal.mouse_report_profile().is_some());
        assert_eq!(terminal.frame().unwrap(), before);
    }
}

#[test]
fn pointer_generation_follows_effective_precedence_and_reset_lifetimes() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    let initial = terminal.mouse_report_generation();
    terminal.feed(b"\x1b[?1006h\x1b[?2004h").unwrap();
    assert_eq!(terminal.mouse_report_generation(), initial);
    terminal.feed(b"\x1b[?1002h").unwrap();
    let drag = terminal.mouse_report_generation();
    assert_eq!(drag, initial + 1);
    terminal
        .feed(b"\x1b[?1002h\x1b[?1000h\x1b[?1005h\x1b[?1015h")
        .unwrap();
    assert_eq!(terminal.mouse_report_generation(), drag);
    terminal.feed(b"\x1b[?1006l").unwrap();
    assert_eq!(terminal.mouse_report_generation(), drag + 1);
    let profile = terminal.mouse_report_profile();
    terminal.feed(b"\x1bc\x1b[?1000;1002;1015h").unwrap();
    assert_eq!(terminal.mouse_report_profile(), profile);
    assert!(terminal.mouse_report_generation() > drag + 1);
    terminal.feed(b"\x1bc").unwrap();
    let reset = terminal.mouse_report_generation();
    terminal.feed(b"\x1bc").unwrap();
    assert_eq!(terminal.mouse_report_generation(), reset + 1);
}

#[test]
fn pointer_generation_retires_gestures_when_the_screen_switches_back_inside_one_feed() {
    let mut terminal = Terminal::new(3, 12).unwrap();
    terminal.feed(b"\x1b[?1002;1006h").unwrap();
    let before = terminal.frame().unwrap();
    let generation = terminal.mouse_report_generation();
    terminal
        .feed(b"\x1b[?1049h\x1b[?1049h\x1b[?1049l\x1b[?1049l")
        .unwrap();
    assert_eq!(
        terminal.frame().unwrap().stamp().screen,
        before.stamp().screen
    );
    assert_eq!(terminal.mouse_report_generation(), generation + 2);
}

#[test]
fn pixel_mouse_requires_opt_in_and_encodes_measured_coordinates_at_every_split() {
    let mut default_terminal = Terminal::new(4, 6).unwrap();
    let enable = b"\x1b[?1003;1006;1016h\x1b[?1016$p\x1b[?1006$p";
    assert_eq!(
        default_terminal.feed(enable).unwrap(),
        b"\x1b[?1016;0$y\x1b[?1006;1$y"
    );
    assert_eq!(
        default_terminal.mouse_report_profile().unwrap().encoding,
        MouseEncoding::Sgr
    );
    for split in 0..=enable.len() {
        let mut terminal = Terminal::new(4, 6).unwrap();
        terminal.enable_pixel_mouse();
        terminal
            .set_window_pixels(
                4,
                6,
                NativeWindowPixels::Viewport {
                    width: 144,
                    height: 192,
                },
            )
            .unwrap();
        assert_eq!(terminal.feed(b"\x1b[?1016$p").unwrap(), b"\x1b[?1016;2$y");
        let mut replies = terminal.feed(&enable[..split]).unwrap();
        replies.extend(terminal.feed(&enable[split..]).unwrap());
        assert_eq!(replies, b"\x1b[?1016;1$y\x1b[?1006;2$y");
        assert_eq!(
            terminal.mouse_report_profile().unwrap().encoding,
            MouseEncoding::SgrPixels
        );
        assert_eq!(terminal.mouse_pixel_extent(), Some((144, 192)));
        for (input, pixel, modifiers, expected) in [
            (MouseInput::Press(0), (0, 0), 0, &b"\x1b[<0;1;1M"[..]),
            (MouseInput::Motion(None), (1, 2), 0, &b"\x1b[<35;2;3M"[..]),
            (
                MouseInput::Motion(Some(1)),
                (11, 23),
                16,
                &b"\x1b[<49;12;24M"[..],
            ),
            (
                MouseInput::Release(2),
                (143, 191),
                8,
                &b"\x1b[<10;144;192m"[..],
            ),
            (MouseInput::WheelDown, (12, 36), 0, &b"\x1b[<65;13;37M"[..]),
        ] {
            assert_eq!(
                terminal
                    .encode_mouse_at(
                        input,
                        MousePosition::Pixel {
                            x: pixel.0,
                            y: pixel.1
                        },
                        modifiers,
                    )
                    .as_deref(),
                Some(expected)
            );
            assert_eq!(
                terminal
                    .encode_mouse_with_pixels(input, 0, 0, modifiers, Some(pixel))
                    .as_deref(),
                Some(expected)
            );
        }
        assert!(terminal
            .encode_mouse(MouseInput::Press(0), 0, 0, 0)
            .is_none());
        for outside in [(144, 0), (0, 192)] {
            assert!(terminal
                .encode_mouse_at(
                    MouseInput::Press(0),
                    MousePosition::Pixel {
                        x: outside.0,
                        y: outside.1
                    },
                    0,
                )
                .is_none());
            assert!(terminal
                .encode_mouse_with_pixels(MouseInput::Press(0), 0, 0, 0, Some(outside))
                .is_none());
        }
    }
}

#[test]
fn pixel_negotiation_and_baseline_reset_preserve_distinct_coordinate_formats() {
    let mut terminal = Terminal::new(4, 6).unwrap();
    terminal.enable_pixel_mouse();
    terminal.feed(b"\x1b[?1002;1016h\x1b[?1016s").unwrap();
    let generation = terminal.mouse_report_generation();
    assert_eq!(
        terminal.feed(b"\x1b[?1006h\x1b[?1016$p").unwrap(),
        b"\x1b[?1016;2$y"
    );
    assert_eq!(
        terminal.encode_mouse(MouseInput::Press(0), 1, 2, 0),
        Some(b"\x1b[<0;2;3M".to_vec())
    );
    assert!(terminal
        .encode_mouse_at(MouseInput::Press(0), MousePosition::Pixel { x: 0, y: 0 }, 0)
        .is_none());
    assert_eq!(terminal.mouse_report_generation(), generation + 1);
    assert_eq!(
        terminal.feed(b"\x1b[?1006r\x1b[?1016$p").unwrap(),
        b"\x1b[?1016;1$y"
    );
    assert_eq!(terminal.mouse_report_generation(), generation + 3);
    assert!(terminal
        .encode_mouse_at(
            MouseInput::Press(0),
            MousePosition::Pixel { x: 50, y: 75 },
            0
        )
        .is_none());
    assert!(terminal
        .encode_mouse_with_pixels(MouseInput::Press(0), 1, 2, 0, Some((50, 75)))
        .is_none());
    terminal.feed(b"\x1b[?1049;1007h\x1b[?1002l").unwrap();
    assert_eq!(
        terminal.encode_mouse(MouseInput::WheelUp, 1, 2, 0),
        Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
        terminal.feed(b"\x1bc\x1b[?1016$p").unwrap(),
        b"\x1b[?1016;2$y"
    );
}

#[test]
fn pixel_only_resize_retires_pointer_generation_after_the_pty_accepts_it() {
    let mut terminal = Terminal::new(4, 6).unwrap();
    terminal.enable_pixel_mouse();
    terminal.feed(b"\x1b[?1003;1016h").unwrap();
    let pixels = NativeWindowPixels::Viewport {
        width: 144,
        height: 192,
    };
    let before = terminal.mouse_report_generation();
    terminal.set_window_pixels(4, 6, pixels).unwrap();
    let layout = terminal.frame().unwrap().stamp().layout;
    assert_eq!(terminal.mouse_report_generation(), before + 1);
    terminal.set_window_pixels(4, 6, pixels).unwrap();
    assert_eq!(terminal.mouse_report_generation(), before + 1);
    let resized = NativeWindowPixels::Cell {
        width: 12,
        height: 24,
    };
    assert!(terminal
        .set_window_pixels_with(4, 6, resized, || Err("resize refused".into()))
        .is_err());
    assert_eq!(terminal.mouse_report_generation(), before + 1);
    assert_eq!(terminal.mouse_pixel_extent(), Some((144, 192)));
    terminal
        .resize_with_window_pixels(4, 6, Some(resized), || Ok(()))
        .unwrap();
    assert_eq!(terminal.frame().unwrap().stamp().layout, layout);
    assert_eq!(terminal.mouse_pixel_extent(), Some((72, 96)));
    assert_eq!(terminal.mouse_report_generation(), before + 2);
    terminal.resize(5, 7).unwrap();
    assert_eq!(terminal.mouse_pixel_extent(), None);
    assert_eq!(terminal.mouse_report_generation(), before + 3);
}

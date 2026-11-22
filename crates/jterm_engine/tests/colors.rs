use jterm_engine::{
    colors::{ColorState, DynamicColorState, TerminalDefaultColors},
    semantics::{Effect, Effects},
};

fn replies(e: Effects) -> Vec<Vec<u8>> {
    e.into_iter()
        .map(|e| match e {
            Effect::Reply(b) => b,
            _ => panic!("unexpected effect"),
        })
        .collect()
}
#[test]
fn parsing_preserves_legacy_ascii_and_rejects_non_ascii() {
    for (s, rgb) in [
        ("#123abc", (18, 58, 188)),
        ("#+1+2+3", (1, 2, 3)),
        ("rgb:1/8/f", (17, 136, 255)),
        ("rgb:12/80/ff", (18, 128, 255)),
        ("rgb:123/800/fff", (18, 128, 255)),
        ("rgb:1234/8000/ffff", (18, 128, 255)),
    ] {
        assert_eq!(ColorState::parse_color_spec(s), Some(rgb), "{s}");
    }
    for s in [
        "#aéabc",
        "#abcéd",
        "#a💡b",
        "#123",
        "red",
        "rgb:1/22/3",
        "rgb:/a/b",
        "rgb:fffff/fffff/fffff",
        "rgb:gg/11/22",
    ] {
        assert_eq!(ColorState::parse_color_spec(s), None, "{s}");
    }
}
#[test]
fn palette_pairs_reply_in_order_with_each_osc_terminator() {
    let mut c = ColorState::default();
    let r = replies(c.apply_ember_osc("4", "1;#123456;1;?;1;#abcdef;1;?;256;?;bad;?;2", b"\x07"));
    assert_eq!(
        r,
        vec![
            b"\x1b]4;1;rgb:1212/3434/5656\x07".to_vec(),
            b"\x1b]4;1;rgb:abab/cdcd/efef\x07".to_vec()
        ]
    );
    assert_eq!(
        replies(c.apply_ember_osc("10", "?", b"\x1b\\")),
        vec![b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\".to_vec()]
    );
}
#[test]
fn theme_subscription_and_osc_background_have_distinct_notification_rules() {
    let mut c = ColorState::default();
    let light = TerminalDefaultColors {
        background: (255, 255, 255),
        ..Default::default()
    };
    assert_eq!(
        replies(c.set_theme(light, true)),
        vec![b"\x1b[?997;2n".to_vec()]
    );
    assert!(c.set_theme(light, true).is_empty());
    assert!(c.apply_ember_osc("11", "#000000", b"\x07").is_empty());
    assert_eq!(c.color_scheme_report(), 1);
    assert!(c
        .set_theme(TerminalDefaultColors::default(), true)
        .is_empty());
    assert!(c.set_theme(light, true).is_empty()); // override masks changed theme
    assert!(c.apply_ember_osc("111", "ignored", b"\x07").is_empty());
    assert_eq!(c.color_scheme_report(), 2);
    assert!(c
        .set_theme(TerminalDefaultColors::default(), false)
        .is_empty());
    assert_eq!(c.color_scheme_report(), 1);
}
#[test]
fn dynamic_reset_preserves_theme_and_all_palette_entries() {
    let mut defaults = TerminalDefaultColors {
        foreground: (1, 2, 3),
        cursor: (4, 5, 6),
        ..Default::default()
    };
    defaults.ansi[0] = (7, 8, 9);
    let mut c = ColorState::new(defaults);
    for command in ["10", "11", "12"] {
        assert!(c.apply_ember_osc(command, "#abcdef", b"\x07").is_empty());
    }
    for i in 0..=255u8 {
        c.apply_ember_osc("4", &format!("{i};#abcdef"), b"\x07");
        assert_eq!(c.palette_color(i), (171, 205, 239));
    }
    c.apply_ember_osc("104", "0;255;bad;256;;", b"\x07");
    assert_eq!(c.palette_color(0), (7, 8, 9));
    assert_eq!(c.palette_color(255), (238, 238, 238));
    assert_eq!(c.palette_color(1), (171, 205, 239));
    c.reset_dynamic();
    assert_eq!(c.defaults(), defaults);
    assert_eq!(c.effective_foreground(), (1, 2, 3));
    assert_eq!(c.effective_cursor(), (4, 5, 6));
    assert!(c.palette_overrides().iter().all(Option::is_none));
    assert_eq!(c.palette_color(16), (0, 0, 0));
    assert_eq!(c.palette_color(231), (255, 255, 255));
}
#[test]
fn malformed_and_unknown_commands_leave_owner_unchanged() {
    let mut c = ColorState::default();
    let before = c.clone();
    for (cmd, val) in [
        ("10", "#aéabc"),
        ("4", "1;#aéabc;999;#123456;1"),
        ("no", "?"),
    ] {
        assert!(c.apply_ember_osc(cmd, val, b"\x07").is_empty());
        assert_eq!(c, before);
    }
}

#[test]
fn frost_rgb_retains_first_component_scaling_and_mixed_widths() {
    for (spec, frost, ember) in [
        ("rgb:8/4/c", Some((8, 4, 12)), Some((136, 68, 204))),
        ("rgb:0100/ffff/0100", Some((0, 255, 0)), Some((1, 255, 1))),
        ("rgb:8080/400/c", Some((128, 3, 0)), None),
        ("rgb:+1/ff/100", Some((1, 255, 0)), None),
        ("rgb:00000/00/ff", Some((0, 0, 255)), None),
        ("rgb:1/2/3/", None, None),
        ("#aéabc", None, None),
    ] {
        assert_eq!(
            DynamicColorState::parse_frost_color_spec(spec),
            frost,
            "{spec}"
        );
        assert_eq!(ColorState::parse_color_spec(spec), ember, "{spec}");
    }
}

#[test]
fn frost_palette_and_default_color_operations_reply_in_input_order() {
    let mut c = DynamicColorState::default();
    let mut defaults = TerminalDefaultColors {
        foreground: (1, 2, 3),
        background: (4, 5, 6),
        cursor: (7, 8, 9),
        ..Default::default()
    };
    defaults.ansi[7] = (0x15, 0x26, 0x37);
    assert_eq!(
        replies(c.apply_frost_osc("4", "7;?;7;rgb:8/4/c;7;?;bad;?;232;?", b"\x07", defaults,)),
        [
            b"\x1b]4;7;rgb:1515/2626/3737\x07".to_vec(),
            b"\x1b]4;7;rgb:0808/0404/0c0c\x07".to_vec(),
            b"\x1b]4;232;rgb:0808/0808/0808\x07".to_vec(),
        ]
    );
    assert!(c
        .apply_frost_osc("104", "7;256;;", b"\x1b\\", defaults)
        .is_empty());
    assert_eq!(c.palette_overrides()[7], None);
    assert!(c
        .apply_frost_osc("11", "rgb:0100/ffff/0100", b"\x07", defaults)
        .is_empty());
    assert_eq!(
        replies(c.apply_frost_osc("11", "?", b"\x1b\\", defaults)),
        [b"\x1b]11;rgb:0000/ffff/0000\x1b\\".to_vec()]
    );
    assert!(c
        .apply_frost_osc("111", "ignored", b"\x07", defaults)
        .is_empty());
    assert_eq!(
        replies(c.apply_frost_osc("11", "?", b"\x07", defaults)),
        [b"\x1b]11;rgb:0404/0505/0606\x07".to_vec()]
    );
    assert_eq!(c, DynamicColorState::default());
}

#[test]
fn individual_resets_preserve_other_colors_and_theme() {
    let mut c = ColorState::default();
    for cmd in ["10", "11", "12"] {
        c.apply_ember_osc(cmd, "#010203", b"\x07");
    }
    c.apply_ember_osc("110", "", b"\x07");
    assert_eq!(c.foreground_override(), None);
    assert_eq!(c.background_override(), Some((1, 2, 3)));
    assert_eq!(c.cursor_override(), Some((1, 2, 3)));
    c.apply_ember_osc("111", "", b"\x07");
    assert_eq!(c.background_override(), None);
    assert_eq!(c.cursor_override(), Some((1, 2, 3)));
    c.apply_ember_osc("112", "", b"\x07");
    assert_eq!(c.cursor_override(), None);
    c.apply_ember_osc("4", "0;#010203;255;#040506", b"\x07");
    c.apply_ember_osc("104", "", b"\x07");
    assert!(c.palette_overrides().iter().all(Option::is_none));
}

#[test]
fn theme_changes_cursor_and_ansi_without_disturbing_overrides() {
    let mut c = ColorState::default();
    let mut t = c.defaults();
    t.cursor = (11, 22, 33);
    t.ansi[3] = (44, 55, 66);
    assert!(c.set_theme(t, true).is_empty()); // same background classification
    assert_eq!(c.effective_cursor(), t.cursor);
    assert_eq!(c.palette_color(3), t.ansi[3]);
    c.apply_ember_osc("12", "#010203", b"\x07");
    c.apply_ember_osc("4", "3;#040506", b"\x07");
    t.cursor = (77, 88, 99);
    t.ansi[3] = (100, 110, 120);
    c.set_theme(t, true);
    assert_eq!(c.effective_cursor(), (1, 2, 3));
    assert_eq!(c.palette_color(3), (4, 5, 6));
    c.apply_ember_osc("112", "", b"\x07");
    c.apply_ember_osc("104", "3", b"\x07");
    assert_eq!(c.effective_cursor(), t.cursor);
    assert_eq!(c.palette_color(3), t.ansi[3]);
    assert!(format!("{:?}", c.defaults()).starts_with("TerminalDefaultColors {"));
}

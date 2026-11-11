use jterm_engine::titles::{FrostTitleState, TitleState};

#[test]
fn concrete_ember_alias_preserves_inference_literals_and_debug() {
    let mut titles = TitleState::default();
    titles.save(0);
    assert_eq!(format!("{titles:?}"), "TitleState { window_title: \"\", icon_title: \"\", title_stack: [(Some(\"\"), Some(\"\"))] }");
    let literal = TitleState {
        window_title: "window".into(),
        ..Default::default()
    };
    assert_eq!(literal.window_title, "window");
    assert_eq!(
        TitleState::reportable_title(&"界".repeat(257))
            .chars()
            .count(),
        200
    );
}

#[test]
fn frost_has_256_scalar_not_byte_limit_and_osc_targets() {
    for n in [0, 1, 199, 200, 201, 255, 256, 257, 512] {
        let value: String = "界🦀a".chars().cycle().take(n).collect();
        let expected: String = value.chars().take(256).collect();
        for command in ["0", "1", "2"] {
            let mut s = FrostTitleState {
                window_title: "window".into(),
                icon_title: "icon".into(),
                ..Default::default()
            };
            s.set_osc(command, &value);
            assert_eq!(
                s.window_title,
                if command == "1" { "window" } else { &expected }
            );
            assert_eq!(
                s.icon_title,
                if command == "2" { "icon" } else { &expected }
            );
        }
    }
}

#[test]
fn frost_stack_shares_eviction_and_unconditional_pop_without_retruncation() {
    let mut s = FrostTitleState::default();
    for n in 0..17 {
        s.set_osc("0", &format!("{n}:{}", "界".repeat(257)));
        s.save(0);
    }
    assert_eq!(s.title_stack.len(), 16);
    for n in (1..17).rev() {
        s.restore(0);
        assert!(s.window_title.starts_with(&format!("{n}:")));
        assert_eq!(s.window_title.chars().count(), 256);
    }
    s.icon_title = "raw".repeat(100);
    s.save(1);
    s.icon_title.clear();
    s.restore(2); // consumes the icon-only entry even though no window was saved
    assert!(s.title_stack.is_empty());
    assert!(s.icon_title.is_empty());
    s.icon_title = "raw".repeat(100);
    s.save(1);
    s.icon_title.clear();
    s.restore(1);
    assert_eq!(s.icon_title.len(), 300); // raw seed restore does not re-sanitize/cap
}

#[test]
fn frost_owner_does_not_claim_host_admission_or_reply_policy() {
    let mut s = FrostTitleState::default();
    s.set_osc("0", "\u{fffd}\u{200b}\u{202e}\x07");
    assert_eq!(s.window_title, "\u{fffd}\u{200b}\u{202e}\x07");
    let before = s.clone();
    for command in ["", "00", "3", "20", "21"] {
        s.set_osc(command, "ignored");
        assert_eq!(s, before);
    }
}

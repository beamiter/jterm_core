use jterm_engine::colors::{
    ColorState, DynamicColorPalette, DynamicColorState, TerminalDefaultColors,
};

// Frozen field shape from e23a2ef, independent of the profiled implementation.
mod legacy {
    use jterm_engine::colors::{DynamicColorPalette, Rgb, TerminalDefaultColors};

    #[allow(dead_code)]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ColorState {
        pub default_colors: TerminalDefaultColors,
        pub dynamic_fg: Option<Rgb>,
        pub dynamic_bg: Option<Rgb>,
        pub dynamic_cursor_color: Option<Rgb>,
        pub dynamic_palette: DynamicColorPalette,
    }
}

#[test]
fn concrete_alias_preserves_inference_literal_and_debug() {
    let mut colors = ColorState::default();
    colors.apply_ember_osc("10", "#123456", b"\x07");
    let defaults = TerminalDefaultColors::default();
    let literal = ColorState {
        default_colors: defaults,
        dynamic_fg: Some((0x12, 0x34, 0x56)),
        dynamic_bg: None,
        dynamic_cursor_color: None,
        dynamic_palette: [None; 256],
    };
    assert_eq!(colors, literal);
    assert_eq!(colors.defaults(), defaults);
    let original = legacy::ColorState {
        default_colors: defaults,
        dynamic_fg: Some((0x12, 0x34, 0x56)),
        dynamic_bg: None,
        dynamic_cursor_color: None,
        dynamic_palette: [None; 256],
    };
    assert_eq!(format!("{colors:?}"), format!("{original:?}"));
    assert_eq!(format!("{colors:#?}"), format!("{original:#?}"));
}

#[test]
fn override_snapshot_is_independent_and_reset_does_not_touch_active() {
    let mut active = DynamicColorState {
        dynamic_fg: Some((1, 2, 3)),
        dynamic_bg: Some((4, 5, 6)),
        dynamic_cursor_color: Some((7, 8, 9)),
        ..Default::default()
    };
    for (idx, slot) in active.dynamic_palette.iter_mut().enumerate() {
        *slot = Some((idx as u8, 255 - idx as u8, 17));
    }
    let mut saved_primary = active.clone();
    active.dynamic_palette[255] = None;
    active.dynamic_fg = None;
    assert_eq!(active.foreground_override(), None);
    assert_eq!(active.palette_overrides()[255], None);
    assert_eq!(saved_primary.foreground_override(), Some((1, 2, 3)));
    assert_eq!(saved_primary.background_override(), Some((4, 5, 6)));
    assert_eq!(saved_primary.cursor_override(), Some((7, 8, 9)));
    assert_eq!(saved_primary.palette_overrides()[255], Some((255, 0, 17)));
    active = saved_primary.clone();
    saved_primary.reset_dynamic();
    assert_eq!(saved_primary, DynamicColorState::default());
    assert_eq!(active.foreground_override(), Some((1, 2, 3)));
    assert_eq!(active.palette_overrides()[255], Some((255, 0, 17)));
    assert_eq!(
        std::mem::size_of::<DynamicColorState>(),
        std::mem::size_of::<DynamicColorPalette>()
            + 3 * std::mem::size_of::<Option<(u8, u8, u8)>>()
    );
}

#[test]
fn reset_retains_ember_defaults_and_palette_borrow() {
    let mut colors = ColorState::new(TerminalDefaultColors {
        foreground: (17, 18, 19),
        ..Default::default()
    });
    colors.apply_ember_osc("4", "255;#123456", b"\x1b\\");
    assert!(std::ptr::eq(
        colors.palette_overrides(),
        &colors.dynamic_palette
    ));
    colors.reset_dynamic();
    assert_eq!(colors.effective_foreground(), (17, 18, 19));
    assert_eq!(colors.dynamic_palette, [None; 256]);
}

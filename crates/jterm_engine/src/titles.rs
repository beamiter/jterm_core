//! Ember title ownership. Hosts sanitize incoming text before admitting it here;
//! application chrome policy and window geometry are deliberately not engine state.
use crate::semantics::{Effect, Effects};

pub const MAX_WINDOW_TITLE_CHARS: usize = 200;
pub const MAX_TITLE_STACK_DEPTH: usize = 16;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TitleState {
    pub window_title: String,
    pub icon_title: String,
    pub title_stack: Vec<(Option<String>, Option<String>)>,
}

impl TitleState {
    /// Accept host-sanitized OSC 0/1/2 text, bounded by Unicode scalar count.
    /// The host retains its trusted-chrome sanitization policy.
    pub fn set_osc(&mut self, command: &str, sanitized: &str) {
        if !matches!(command, "0" | "1" | "2") {
            return;
        }
        let value: String = sanitized.chars().take(MAX_WINDOW_TITLE_CHARS).collect();
        if command != "1" {
            self.window_title.clone_from(&value);
        }
        if command != "2" {
            self.icon_title = value;
        }
    }

    /// 1 saves icon only, 2 window only, every other target saves both.
    pub fn save(&mut self, target: u16) {
        if self.title_stack.len() >= MAX_TITLE_STACK_DEPTH {
            self.title_stack.remove(0);
        }
        self.title_stack.push(match target {
            1 => (Some(self.icon_title.clone()), None),
            2 => (None, Some(self.window_title.clone())),
            _ => (
                Some(self.icon_title.clone()),
                Some(self.window_title.clone()),
            ),
        });
    }

    /// Pop is unconditional even if the selected field was not saved.
    pub fn restore(&mut self, target: u16) {
        let Some((icon, window)) = self.title_stack.pop() else {
            return;
        };
        if target != 2 {
            if let Some(icon) = icon {
                self.icon_title = icon;
            }
        }
        if target != 1 {
            if let Some(window) = window {
                self.window_title = window;
            }
        }
    }

    /// Only title-related XTWINOPS are handled. Geometry remains host-owned.
    /// Reports always use ST, independent of the most recent OSC terminator.
    pub fn apply_window_op(&mut self, params: &[u16]) -> Effects {
        let mut effects = Effects::new();
        match params.first().copied().unwrap_or(0) {
            20 => effects.push(Effect::Reply(
                format!("\x1b]L{}\x1b\\", Self::reportable_title(&self.icon_title)).into_bytes(),
            )),
            21 => effects.push(Effect::Reply(
                format!("\x1b]l{}\x1b\\", Self::reportable_title(&self.window_title)).into_bytes(),
            )),
            22 => self.save(params.get(1).copied().unwrap_or(0)),
            23 => self.restore(params.get(1).copied().unwrap_or(0)),
            _ => {}
        }
        effects
    }

    /// Preserve Ember's reply-framing filter, not the host's ingest policy.
    pub fn reportable_title(title: &str) -> String {
        title.chars().filter(|&ch| !ch.is_control() && !matches!(ch,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )).take(MAX_WINDOW_TITLE_CHARS).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn seeded() -> TitleState {
        TitleState {
            icon_title: "icon".into(),
            window_title: "window".into(),
            ..Default::default()
        }
    }
    #[test]
    fn osc_targets_and_scalar_limit() {
        for (command, icon, window) in [
            ("0", "new", "new"),
            ("1", "new", "window"),
            ("2", "icon", "new"),
            ("3", "icon", "window"),
            ("00", "icon", "window"),
            ("", "icon", "window"),
        ] {
            let mut s = seeded();
            s.set_osc(command, "new");
            assert_eq!((&*s.icon_title, &*s.window_title), (icon, window));
        }
        for n in [0, 1, 199, 200, 201, 400] {
            let mut s = seeded();
            let input: String = "界🦀".chars().cycle().take(n).collect();
            s.set_osc("0", &input);
            assert_eq!(s.window_title.chars().count(), n.min(200));
            assert_eq!(s.icon_title, s.window_title);
        }
    }
    #[test]
    fn all_u16_targets_match_ember_and_pop_consumes_mismatches() {
        for target in 0..=u16::MAX {
            let mut s = seeded();
            s.save(target);
            assert_eq!(
                s.title_stack[0],
                (
                    (target != 2).then(|| "icon".into()),
                    (target != 1).then(|| "window".into())
                )
            );
            s.set_osc("0", "changed");
            s.restore(target);
            assert!(s.title_stack.is_empty());
            assert_eq!(s.icon_title, if target == 2 { "changed" } else { "icon" });
            assert_eq!(
                s.window_title,
                if target == 1 { "changed" } else { "window" }
            );
        }
        for saved in [0, 1, 2, 3, u16::MAX] {
            for restored in [0, 1, 2, 3, u16::MAX] {
                let mut s = seeded();
                s.save(saved);
                s.set_osc("0", "changed");
                s.restore(restored);
                assert!(s.title_stack.is_empty());
                assert_eq!(
                    s.icon_title,
                    if saved != 2 && restored != 2 {
                        "icon"
                    } else {
                        "changed"
                    }
                );
                assert_eq!(
                    s.window_title,
                    if saved != 1 && restored != 1 {
                        "window"
                    } else {
                        "changed"
                    }
                );
                let before = s.clone();
                s.restore(restored);
                assert_eq!(s, before);
            }
        }
    }
    #[test]
    fn stack_is_fifo_eviction_lifo_restore_bounded_at_sixteen() {
        let mut s = TitleState::default();
        for n in 0..40 {
            s.set_osc("0", &n.to_string());
            s.save(0);
            assert_eq!(s.title_stack.len(), (n + 1).min(16));
        }
        for n in (24..40).rev() {
            s.restore(0);
            assert_eq!(s.window_title, n.to_string());
            assert_eq!(s.icon_title, s.window_title);
        }
        assert!(s.title_stack.is_empty());
    }
    #[test]
    fn reports_filter_controls_and_bidi_then_cap_scalars() {
        let controls: String = (0..=0x9f)
            .filter_map(char::from_u32)
            .filter(|c| c.is_control())
            .collect();
        let bidi = "\u{061c}\u{200e}\u{200f}\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}";
        let input = format!("{controls}{bidi}{}", "界🦀".repeat(150));
        assert_eq!(TitleState::reportable_title(&input), "界🦀".repeat(100));
        // Non-bidi format characters are preserved by the original report filter.
        assert_eq!(
            TitleState::reportable_title("a\u{200b}\u{200d}\u{feff}b"),
            "a\u{200b}\u{200d}\u{feff}b"
        );
        let mut s = seeded();
        s.icon_title = input;
        s.window_title = "x\x1b\\\x07\n\u{202e}y".into();
        assert_eq!(
            s.apply_window_op(&[20]).as_slice(),
            &[Effect::Reply(
                format!("\x1b]L{}\x1b\\", "界🦀".repeat(100)).into_bytes()
            )]
        );
        assert_eq!(
            s.apply_window_op(&[21]).as_slice(),
            &[Effect::Reply(b"\x1b]lx\\y\x1b\\".to_vec())]
        );
    }
    #[test]
    fn window_ops_default_targets_and_non_title_ops() {
        let mut s = seeded();
        assert!(s.apply_window_op(&[22]).is_empty());
        s.set_osc("0", "changed");
        assert!(s.apply_window_op(&[23]).is_empty());
        assert_eq!(s, seeded());
        for op in [0, 1, 2, 11, 13, 14, 16, 18, 19, 24, u16::MAX] {
            let before = s.clone();
            assert!(s.apply_window_op(&[op, 1, 2]).is_empty());
            assert_eq!(s, before);
        }
        assert!(s.apply_window_op(&[]).is_empty());
    }
    #[test]
    fn ingest_is_explicitly_host_sanitized_not_new_engine_policy() {
        let mut s = seeded();
        let text = "host\u{fffd}界";
        s.set_osc("0", text);
        assert_eq!(s.window_title, text);
        assert_eq!(s.icon_title, text);
        // The owner does not silently invent a second ingest sanitizer.
        s.set_osc("2", "\u{200b}");
        assert_eq!(s.window_title, "\u{200b}");
        assert_eq!(format!("{:?}", s.title_stack), "[]");
    }
}

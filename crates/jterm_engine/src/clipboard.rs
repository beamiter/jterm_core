//! OSC 52 admission and pending effects. Clipboard access and authorization stay
//! with the host; parsing PTY output never reads or writes the host clipboard.
use std::collections::VecDeque;

pub const MAX_CLIPBOARD_BYTES: usize = 100 * 1024;
pub const MAX_CLIPBOARD_QUERIES: usize = 8;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Osc52Clipboard {
    /// Latest accepted write; retained if a later request is invalid.
    pub pending_set: Option<String>,
    /// FIFO read requests, including each request's reply terminator.
    pub pending_queries: VecDeque<&'static [u8]>,
}

impl Osc52Clipboard {
    /// Native hosts historically alias every selection parameter to clipboard.
    /// Codec and invisible-character policy are supplied by the host so this
    /// extraction preserves its exact acceptance boundary.
    pub fn admit(
        &mut self,
        value: &str,
        terminator: &'static [u8],
        decode: impl FnOnce(&str) -> Option<String>,
        is_visual_spoofing: impl Fn(char) -> bool,
    ) {
        let Some((_, data)) = value.split_once(';') else {
            return;
        };
        if data == "?" {
            if self.pending_queries.len() < MAX_CLIPBOARD_QUERIES {
                self.pending_queries.push_back(terminator);
            }
        } else if !data.is_empty() && data.len() <= MAX_CLIPBOARD_BYTES * 4 / 3 + 8 {
            if let Some(text) = decode(data) {
                if text.len() <= MAX_CLIPBOARD_BYTES
                    && !text.chars().any(|ch| {
                        !matches!(ch, '\n' | '\t' | '\r')
                            && (ch.is_control() || ch == '\u{fffd}' || is_visual_spoofing(ch))
                    })
                {
                    self.pending_set = Some(text);
                }
            }
        }
    }

    pub fn take_set(&mut self) -> Option<String> {
        self.pending_set.take()
    }
    pub fn take_query(&mut self) -> Option<&'static [u8]> {
        self.pending_queries.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fifo_limit_terminator_and_write_coalescing_are_independent() {
        let mut state = Osc52Clipboard::default();
        for i in 0..10 {
            state.admit(
                "arbitrary;?",
                if i % 2 == 0 { b"\x07" } else { b"\x1b\\" },
                |_| panic!("query decoded"),
                |_| false,
            );
        }
        for i in 0..8 {
            assert_eq!(
                state.take_query(),
                Some(if i % 2 == 0 {
                    &b"\x07"[..]
                } else {
                    &b"\x1b\\"[..]
                })
            );
        }
        assert_eq!(state.take_query(), None);
        state.admit("c;first", b"\x07", |s| Some(s.into()), |_| false);
        state.admit("p;second", b"\x07", |s| Some(s.into()), |_| false);
        state.admit("s;bad", b"\x07", |_| None, |_| false);
        state.admit("c;", b"\x07", |_| panic!("empty decoded"), |_| false);
        assert_eq!(state.take_set().as_deref(), Some("second"));
        assert_eq!(state.take_set(), None);
    }
    #[test]
    fn admission_bounds_and_unsafe_text_preserve_last_valid_effect() {
        let mut state = Osc52Clipboard::default();
        state.admit(
            "c;x",
            b"\x07",
            |_| Some("x".repeat(MAX_CLIPBOARD_BYTES)),
            |_| false,
        );
        assert_eq!(
            state.pending_set.as_ref().unwrap().len(),
            MAX_CLIPBOARD_BYTES
        );
        for text in [
            "x".repeat(MAX_CLIPBOARD_BYTES + 1),
            "bad\x1b".into(),
            "bad\u{fffd}".into(),
            "bad\u{202e}".into(),
        ] {
            state.admit("c;x", b"\x07", |_| Some(text), |c| c == '\u{202e}');
            assert_eq!(
                state.pending_set.as_ref().unwrap().len(),
                MAX_CLIPBOARD_BYTES
            );
        }
        let oversized = format!("c;{}", "x".repeat(MAX_CLIPBOARD_BYTES * 4 / 3 + 9));
        state.admit(
            &oversized,
            b"\x07",
            |_| panic!("oversize decoded"),
            |_| false,
        );
        state.admit("c;x", b"\x07", |_| Some("\r\n\t好".into()), |_| false);
        assert_eq!(state.take_set().as_deref(), Some("\r\n\t好"));
    }
}

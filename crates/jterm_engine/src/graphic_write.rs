//! Ordered native scalar writes and REP. Each yielded effect is synchronous:
//! consume it before the next step, including wrap/history/provenance effects.
//! Width policy stays an injected native cache. ASCII parser chunks keep their
//! existing separate transaction boundary.
use crate::screen::{RowMutation, ScreenState};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicProfile {
    Ember,
    FrostLegacy,
}

#[derive(Debug)]
pub enum GraphicStep {
    Wrap,
    ObserveIdle {
        columns: Range<usize>,
        nonblank: bool,
    },
    Composed(RowMutation),
    Written {
        mutation: RowMutation,
        columns: Range<usize>,
    },
}

/// One translated scalar. Ember's original scalar is committed as REP source
/// only after its row mutation callback. Frost commits its translated source
/// during the cell mutation, before dirty/echo callbacks, as historically.
pub struct GraphicWrite {
    profile: GraphicProfile,
    original: char,
    translated: char,
    width: usize,
    autowrap: bool,
    track_idle: bool,
    phase: u8,
}

impl GraphicWrite {
    pub fn new(
        profile: GraphicProfile,
        screen: &ScreenState,
        original: char,
        autowrap: bool,
        track_idle: bool,
        width: impl FnMut(char) -> usize,
    ) -> Self {
        Self::new_translated(
            profile,
            original,
            screen.translate_char(original),
            autowrap,
            track_idle,
            width,
        )
    }

    pub(crate) fn new_translated(
        profile: GraphicProfile,
        original: char,
        translated: char,
        autowrap: bool,
        track_idle: bool,
        mut width: impl FnMut(char) -> usize,
    ) -> Self {
        Self {
            profile,
            original,
            translated,
            width: width(translated),
            autowrap,
            track_idle,
            phase: 0,
        }
    }

    pub fn step(&mut self, screen: &mut ScreenState) -> Option<GraphicStep> {
        loop {
            match self.phase {
                0 => {
                    self.phase = 1;
                    if self.width == 0 {
                        self.phase = 5;
                        return match self.profile {
                            GraphicProfile::Ember => screen.compose_combining_mark(self.translated),
                            GraphicProfile::FrostLegacy => {
                                screen.compose_frost_legacy_mark(self.translated)
                            }
                        }
                        .map(GraphicStep::Composed);
                    }
                    if screen.take_pending_wrap(self.autowrap) {
                        return Some(GraphicStep::Wrap);
                    }
                }
                1 => {
                    self.phase = 2;
                    if screen.prepare_character_width(self.width, self.autowrap) {
                        return Some(GraphicStep::Wrap);
                    }
                }
                2 => {
                    self.phase = 3;
                    if self.profile == GraphicProfile::FrostLegacy && self.track_idle {
                        return Some(GraphicStep::ObserveIdle {
                            columns: screen.cursor_col
                                ..screen.cursor_col.saturating_add(self.width),
                            nonblank: !self.translated.is_whitespace(),
                        });
                    }
                }
                3 => {
                    self.phase = 4;
                    let columns = screen.cursor_col..screen.cursor_col.saturating_add(self.width);
                    let mutation = match self.profile {
                        GraphicProfile::Ember => {
                            screen.write_char_in_row(self.translated, self.width, self.autowrap)
                        }
                        GraphicProfile::FrostLegacy => screen.write_frost_legacy_char_in_row(
                            self.translated,
                            self.width,
                            self.autowrap,
                        ),
                    };
                    return Some(GraphicStep::Written { mutation, columns });
                }
                4 => {
                    self.phase = 5;
                    if self.profile == GraphicProfile::Ember {
                        screen.last_printed_char = Some(self.original);
                    }
                }
                _ => return None,
            }
        }
    }
}

/// REP captures its source once, even when a repeated write changes the live
/// source through charset translation. Namespace/intermediate permissiveness
/// deliberately matches both native legacy dispatchers.
pub struct RepeatGraphics {
    original: Option<char>,
    remaining: usize,
}

impl RepeatGraphics {
    pub fn prepare(screen: &ScreenState, params: &[u16], command: char) -> Option<Self> {
        (command == 'b').then(|| Self {
            original: screen.last_printed_char,
            remaining: usize::from(params.first().copied().unwrap_or(1).max(1)),
        })
    }

    pub fn next(
        &mut self,
        profile: GraphicProfile,
        screen: &ScreenState,
        autowrap: bool,
        width: impl FnMut(char) -> usize,
    ) -> Option<GraphicWrite> {
        let original = self.original?;
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(GraphicWrite::new(
            profile, screen, original, autowrap, false, width,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{Color, RawRowId};
    use crate::screen::{Charset, VerticalMotion};

    #[test]
    fn translated_input_preserves_original_and_active_bank() {
        let mut screen = ScreenState::new(2, 8);
        screen.active_charset = Charset::Ascii;
        let mut observed = None;
        let write = GraphicWrite::new_translated(
            GraphicProfile::Ember,
            'q',
            '─',
            true,
            false,
            |character| {
                observed = Some(character);
                1
            },
        );
        assert_eq!(observed, Some('─'));
        assert_eq!(write.original, 'q');
        assert_eq!(write.translated, '─');
        assert_eq!(screen.active_charset, Charset::Ascii);
        let ordinary = GraphicWrite::new(GraphicProfile::Ember, &screen, 'q', true, false, |_| 1);
        assert_eq!(ordinary.translated, 'q');
    }

    fn width(ch: char) -> usize {
        match ch {
            '\u{301}' => 0,
            '界' => 2,
            '\u{17d8}' => 3,
            _ => 1,
        }
    }
    fn snapshot(s: &ScreenState) -> String {
        format!(
            "{:?}",
            (
                s.grid.to_vec(),
                &s.grid.row_wrapped,
                s.cursor_row,
                s.cursor_col,
                s.pending_wrap,
                s.last_printed_char,
                s.grid.identity_revision()
            )
        )
    }
    fn host_wrap(s: &mut ScreenState) {
        s.vertical_motion(VerticalMotion::Wrap, false, || RawRowId::UNTRACKED);
    }
    fn execute(
        profile: GraphicProfile,
        s: &mut ScreenState,
        ch: char,
        wrap: bool,
        track: bool,
    ) -> Vec<String> {
        let mut operation = GraphicWrite::new(profile, s, ch, wrap, track, width);
        let mut trace = Vec::new();
        while let Some(step) = operation.step(s) {
            trace.push(format!("{step:?}:{}", snapshot(s)));
            if matches!(step, GraphicStep::Wrap) {
                host_wrap(s);
            }
        }
        trace
    }
    // Frozen orchestration from the two native put_char bodies, independent of
    // GraphicWrite's state machine. Physical primitives were already shared.
    fn frozen(
        profile: GraphicProfile,
        s: &mut ScreenState,
        original: char,
        wrap: bool,
        track: bool,
    ) -> Vec<String> {
        let ch = s.translate_char(original);
        let width = width(ch);
        let mut trace = Vec::new();
        if width == 0 {
            let effect = match profile {
                GraphicProfile::Ember => s.compose_combining_mark(ch),
                GraphicProfile::FrostLegacy => s.compose_frost_legacy_mark(ch),
            };
            if let Some(m) = effect {
                trace.push(format!("{:?}:{}", GraphicStep::Composed(m), snapshot(s)));
            }
            return trace;
        }
        if s.take_pending_wrap(wrap) {
            trace.push(format!("Wrap:{}", snapshot(s)));
            host_wrap(s);
        }
        if s.prepare_character_width(width, wrap) {
            trace.push(format!("Wrap:{}", snapshot(s)));
            host_wrap(s);
        }
        if profile == GraphicProfile::FrostLegacy && track {
            trace.push(format!(
                "{:?}:{}",
                GraphicStep::ObserveIdle {
                    columns: s.cursor_col..s.cursor_col.saturating_add(width),
                    nonblank: !ch.is_whitespace(),
                },
                snapshot(s)
            ));
        }
        let columns = s.cursor_col..s.cursor_col.saturating_add(width);
        let mutation = match profile {
            GraphicProfile::Ember => s.write_char_in_row(ch, width, wrap),
            GraphicProfile::FrostLegacy => s.write_frost_legacy_char_in_row(ch, width, wrap),
        };
        trace.push(format!(
            "{:?}:{}",
            GraphicStep::Written { mutation, columns },
            snapshot(s)
        ));
        if profile == GraphicProfile::Ember {
            s.last_printed_char = Some(original);
        }
        trace
    }
    #[test]
    fn graphic_transactions_match_frozen_native_order() {
        let mut cases = 0;
        for profile in [GraphicProfile::Ember, GraphicProfile::FrostLegacy] {
            for cols in [1, 2, 7] {
                for seed in 0..32 {
                    let make = || {
                        let mut s = ScreenState::new(3, cols);
                        s.cursor_row = 2;
                        s.cursor_col = cols - 1;
                        s.pending_wrap = seed & 1 != 0;
                        s.insert_mode = seed & 2 != 0;
                        if seed & 4 != 0 {
                            s.active_charset = Charset::DecSpecialGraphics;
                        }
                        s.current_bg = Color::Indexed(4);
                        s.grid[2][0].character = 'e';
                        s.last_printed_char = Some('X');
                        s
                    };
                    let mut actual = make();
                    let mut expected = make();
                    for ch in ['q', '界', '\u{301}', ' ', '\u{17d8}'] {
                        if ch == '\u{17d8}' && profile == GraphicProfile::Ember {
                            continue;
                        }
                        let a = execute(profile, &mut actual, ch, seed & 8 != 0, seed & 16 != 0);
                        let e = frozen(profile, &mut expected, ch, seed & 8 != 0, seed & 16 != 0);
                        assert_eq!(a, e, "{profile:?} cols={cols} seed={seed} ch={ch:?}");
                        assert_eq!(snapshot(&actual), snapshot(&expected));
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 864);
    }

    #[test]
    fn repeat_captures_source_once_and_normalizes_default_zero_counts() {
        for params in [&[][..], &[0][..], &[3][..]] {
            let mut s = ScreenState::new(2, 7);
            s.last_printed_char = Some('q');
            s.active_charset = Charset::DecSpecialGraphics;
            let mut repeat = RepeatGraphics::prepare(&s, params, 'b').unwrap();
            let mut writes = 0;
            while let Some(mut write) = repeat.next(GraphicProfile::FrostLegacy, &s, true, width) {
                while let Some(step) = write.step(&mut s) {
                    if matches!(step, GraphicStep::Wrap) {
                        host_wrap(&mut s);
                    }
                }
                // If REP read its live source again it would now print Z.
                s.last_printed_char = Some('Z');
                writes += 1;
            }
            let n = usize::from(params.first().copied().unwrap_or(1).max(1));
            assert_eq!(writes, n);
            assert!(s.grid[0][..n].iter().all(|c| c.character == '─'));
        }
        let s = ScreenState::new(1, 1);
        let mut repeat = RepeatGraphics::prepare(&s, &[65535], 'b').unwrap();
        assert!(repeat
            .next(GraphicProfile::Ember, &s, true, width)
            .is_none());
        assert!(RepeatGraphics::prepare(&s, &[1], 'x').is_none());
    }
}

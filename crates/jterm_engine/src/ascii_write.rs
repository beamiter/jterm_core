//! One native printable ASCII parser chunk. Consume every yielded effect before
//! the next step. This operation does not combine parser events or add watches.
use crate::graphic_write::{GraphicProfile, GraphicWrite};
use crate::screen::{AsciiWriteStep, RowMutation, ScreenState};
use std::ops::Range;

pub enum NativeAsciiStep {
    Wrap,
    ObserveIdle {
        columns: Range<usize>,
        nonblank: bool,
    },
    Written {
        mutation: RowMutation,
        columns: Range<usize>,
    },
    Graphic(GraphicWrite),
}

pub struct NativeAsciiWrite<'a> {
    profile: GraphicProfile,
    bytes: &'a [u8],
    cols: usize,
    pos: usize,
    autowrap: bool,
    scalar_fallback: bool,
    phase: u8,
    chunk_len: usize,
}

impl<'a> NativeAsciiWrite<'a> {
    /// Input retains the native printable-ASCII admission contract. Frost's
    /// parser excludes insert mode; Ember historically checks it once per run.
    /// Frost commits REP for the complete parser chunk before the first wrap.
    pub fn new(
        profile: GraphicProfile,
        screen: &mut ScreenState,
        bytes: &'a [u8],
        autowrap: bool,
    ) -> Self {
        if profile == GraphicProfile::FrostLegacy {
            if let Some(&last) = bytes.last() {
                screen.last_printed_char = Some(last as char);
            }
        }
        Self {
            profile,
            bytes,
            cols: screen.grid.row_len(),
            pos: 0,
            autowrap,
            scalar_fallback: profile == GraphicProfile::Ember && screen.insert_mode,
            phase: 0,
            chunk_len: 0,
        }
    }

    /// Only Ember's scalar fallback samples live DECAWM/width for each byte,
    /// exactly as the original put_char loop. Fast lanes latch DECAWM per run.
    pub fn step(
        &mut self,
        screen: &mut ScreenState,
        live_autowrap: bool,
        width: impl FnMut(char) -> usize,
    ) -> Option<NativeAsciiStep> {
        if self.pos == self.bytes.len() {
            return None;
        }
        if self.scalar_fallback {
            let ch = self.bytes[self.pos] as char;
            self.pos += 1;
            return Some(NativeAsciiStep::Graphic(GraphicWrite::new(
                self.profile,
                screen,
                ch,
                live_autowrap,
                false,
                width,
            )));
        }
        if self.profile == GraphicProfile::Ember {
            return match screen.step_ascii_write(&self.bytes[self.pos..], self.autowrap) {
                AsciiWriteStep::NeedsWrap => Some(NativeAsciiStep::Wrap),
                AsciiWriteStep::Written { consumed, mutation } => {
                    self.pos += consumed;
                    let columns = mutation
                        .printed
                        .clone()
                        .expect("ASCII write owns its printed span");
                    Some(NativeAsciiStep::Written { mutation, columns })
                }
                AsciiWriteStep::Stopped => {
                    self.pos = self.bytes.len();
                    None
                }
            };
        }
        loop {
            match self.phase {
                0 => {
                    self.phase = 1;
                    if screen.take_pending_wrap(self.autowrap) {
                        return Some(NativeAsciiStep::Wrap);
                    }
                }
                1 => {
                    self.phase = 2;
                    self.chunk_len =
                        (self.bytes.len() - self.pos).min(self.cols - screen.cursor_col);
                    let chunk = &self.bytes[self.pos..self.pos + self.chunk_len];
                    return Some(NativeAsciiStep::ObserveIdle {
                        columns: screen.cursor_col..screen.cursor_col + self.chunk_len,
                        nonblank: chunk.iter().any(|byte| !byte.is_ascii_whitespace()),
                    });
                }
                _ => {
                    let columns = screen.cursor_col..screen.cursor_col + self.chunk_len;
                    let (consumed, mutation) = screen
                        .write_frost_legacy_ascii_in_row(
                            &self.bytes[self.pos..self.pos + self.chunk_len],
                            self.autowrap,
                        )
                        .expect("nonempty ASCII chunk within the current row");
                    self.pos += consumed;
                    self.phase = 0;
                    return Some(NativeAsciiStep::Written { mutation, columns });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphic_write::GraphicStep;
    use crate::grid::RawRowId;
    use crate::screen::VerticalMotion;

    fn snapshot(s: &ScreenState) -> String {
        format!(
            "{:?}",
            (
                s.grid.to_vec(),
                &s.grid.row_wrapped,
                s.cursor_row,
                s.cursor_col,
                s.pending_wrap,
                s.last_printed_char
            )
        )
    }
    fn wrap(s: &mut ScreenState) {
        s.vertical_motion(VerticalMotion::Wrap, false, || RawRowId::UNTRACKED);
    }
    fn graphic(s: &mut ScreenState, mut op: GraphicWrite) {
        while let Some(effect) = op.step(s) {
            if matches!(effect, GraphicStep::Wrap) {
                wrap(s);
            }
        }
    }
    fn actual(
        s: &mut ScreenState,
        bytes: &[u8],
        profile: GraphicProfile,
        autowrap: bool,
    ) -> Vec<String> {
        let mut op = NativeAsciiWrite::new(profile, s, bytes, autowrap);
        let mut trace = vec![];
        while let Some(effect) = op.step(s, autowrap, |_| 1) {
            match effect {
                NativeAsciiStep::Wrap => {
                    trace.push(format!("wrap:{}", snapshot(s)));
                    wrap(s);
                }
                NativeAsciiStep::ObserveIdle { columns, nonblank } => {
                    trace.push(format!("idle:{columns:?}:{nonblank}:{}", snapshot(s)))
                }
                NativeAsciiStep::Written { .. } => trace.push(format!("write:{}", snapshot(s))),
                NativeAsciiStep::Graphic(write) => graphic(s, write),
            }
        }
        trace
    }
    // Independently frozen native run loops. Their already-shared row/scalar
    // primitives stay fixed; NativeAsciiWrite never runs in this reference.
    fn frozen(
        s: &mut ScreenState,
        bytes: &[u8],
        profile: GraphicProfile,
        autowrap: bool,
    ) -> Vec<String> {
        let mut trace = vec![];
        if profile == GraphicProfile::Ember && s.insert_mode {
            for &byte in bytes {
                let op = GraphicWrite::new(profile, s, byte as char, autowrap, false, |_| 1);
                graphic(s, op);
            }
            return trace;
        }
        let mut pos = 0;
        if profile == GraphicProfile::FrostLegacy {
            if let Some(&last) = bytes.last() {
                s.last_printed_char = Some(last as char);
            }
        }
        while pos < bytes.len() {
            if profile == GraphicProfile::Ember {
                match s.step_ascii_write(&bytes[pos..], autowrap) {
                    AsciiWriteStep::NeedsWrap => {
                        trace.push(format!("wrap:{}", snapshot(s)));
                        wrap(s);
                    }
                    AsciiWriteStep::Written { consumed, .. } => {
                        trace.push(format!("write:{}", snapshot(s)));
                        pos += consumed;
                    }
                    AsciiWriteStep::Stopped => break,
                }
            } else {
                if s.take_pending_wrap(autowrap) {
                    trace.push(format!("wrap:{}", snapshot(s)));
                    wrap(s);
                }
                let n = (bytes.len() - pos).min(s.grid.row_len() - s.cursor_col);
                let chunk = &bytes[pos..pos + n];
                let columns = s.cursor_col..s.cursor_col + n;
                let nonblank = chunk.iter().any(|byte| !byte.is_ascii_whitespace());
                trace.push(format!("idle:{columns:?}:{nonblank}:{}", snapshot(s)));
                let (consumed, _) = s.write_frost_legacy_ascii_in_row(chunk, autowrap).unwrap();
                pos += consumed;
                trace.push(format!("write:{}", snapshot(s)));
            }
        }
        trace
    }
    #[test]
    fn native_ascii_batches_match_frozen_chunk_and_effect_order() {
        let bytes = b"a bCDefgh ijklMNopqrst";
        let mut cases = 0;
        for profile in [GraphicProfile::Ember, GraphicProfile::FrostLegacy] {
            for cols in [1, 2, 7] {
                for seed in 0..8 {
                    for split in 0..=bytes.len() {
                        let make = || {
                            let mut s = ScreenState::new(3, cols);
                            s.cursor_row = 2;
                            s.cursor_col = cols - 1;
                            s.pending_wrap = seed & 1 != 0;
                            s.insert_mode = profile == GraphicProfile::Ember && seed & 2 != 0;
                            s.last_printed_char = Some('X');
                            s
                        };
                        let mut a = make();
                        let mut e = make();
                        for chunk in [&bytes[..split], &bytes[split..]] {
                            assert_eq!(
                                actual(&mut a, chunk, profile, seed & 4 != 0),
                                frozen(&mut e, chunk, profile, seed & 4 != 0),
                                "{profile:?} cols={cols} seed={seed} split={split}"
                            );
                            assert_eq!(snapshot(&a), snapshot(&e));
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 2208);
    }
}

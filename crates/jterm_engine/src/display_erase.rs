//! Ordered native ED/DECSED admission and mutations. Hosts consume each step
//! synchronously before requesting another step or feeding another event.
use crate::screen::{RowMutation, ScreenState};
use smallvec::SmallVec;
use std::ops::RangeInclusive;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayEraseProfile {
    Ember,
    FrostLegacy,
}

#[derive(Debug)]
pub enum DisplayErase {
    Rows(DisplayEraseRows),
    /// Ordinary ED2 keeps the host's history/graphics/provenance transaction.
    Screen,
    /// ED3 keeps the host's retained-reader and inline-CLI policy.
    SavedLines,
    Ignored,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "keep selective row mutations inline to avoid allocating a box per erased row"
)]
pub enum DisplayEraseStep {
    ScreenErased,
    /// One Ember row, including all eligible selective-erasure runs. Apply
    /// every returned mutation before proceeding to the next physical row.
    Mutations(SmallVec<[RowMutation; 4]>),
    /// Frost dirties the whole affected range after its cell transaction.
    DirtyRows(RangeInclusive<usize>),
}

/// Transient command progress, not a second screen. Geometry stays fixed until
/// completion; authoritative cells and pending-wrap state live in ScreenState.
#[derive(Debug)]
pub struct DisplayEraseRows {
    profile: DisplayEraseProfile,
    mode: u16,
    selective: bool,
    cursor: (usize, usize),
    columns: usize,
    next_row: usize,
    end_row: usize,
    notify_erased: bool,
    reset_wrap: bool,
    complete: bool,
}

/// Decode the original profile's admission without mutating any state. Frost
/// intentionally retains its permissive namespace/intermediate handling.
pub fn prepare_display_erase(
    profile: DisplayEraseProfile,
    screen: &ScreenState,
    params: &[u16],
    cmd: char,
    private_prefix: Option<u8>,
    intermediates: &[u8],
) -> Option<DisplayErase> {
    if cmd != 'J'
        || (profile == DisplayEraseProfile::Ember
            && (!intermediates.is_empty() || !matches!(private_prefix, None | Some(b'?'))))
    {
        return None;
    }
    let selective = profile == DisplayEraseProfile::Ember && private_prefix == Some(b'?');
    let mode = params.first().copied().unwrap_or(0);
    match mode {
        0 | 1 => {}
        2 if selective => {}
        2 => return Some(DisplayErase::Screen),
        3 => return Some(DisplayErase::SavedLines),
        _ => return Some(DisplayErase::Ignored),
    }
    let (next_row, end_row) = match mode {
        0 => (screen.cursor_row, screen.grid.rows()),
        1 => (0, screen.cursor_row.saturating_add(1)),
        _ => (0, screen.grid.rows()),
    };
    Some(DisplayErase::Rows(DisplayEraseRows {
        profile,
        mode,
        selective,
        cursor: (screen.cursor_row, screen.cursor_col),
        columns: screen.grid.row_len(),
        next_row,
        end_row,
        notify_erased: mode == 0 && !selective && screen.cursor_row == 0 && screen.cursor_col == 0,
        reset_wrap: profile == DisplayEraseProfile::Ember && mode == 0,
        complete: false,
    }))
}

impl DisplayEraseRows {
    pub fn step(&mut self, screen: &mut ScreenState) -> Option<DisplayEraseStep> {
        if self.complete {
            return None;
        }
        if self.notify_erased {
            self.notify_erased = false;
            return Some(DisplayEraseStep::ScreenErased);
        }
        if self.reset_wrap {
            screen.pending_wrap = false;
            self.reset_wrap = false;
        }
        if self.profile == DisplayEraseProfile::FrostLegacy {
            self.complete = true;
            return screen
                .erase_frost_legacy_display(self.mode)
                .map(DisplayEraseStep::DirtyRows);
        }
        if self.next_row >= self.end_row {
            self.complete = true;
            return None;
        }
        let row = self.next_row;
        self.next_row += 1;
        let start = if self.mode == 0 && row == self.cursor.0 {
            self.cursor.1
        } else {
            0
        };
        let end = if self.mode == 1 && row == self.cursor.0 {
            self.cursor.1.saturating_add(1)
        } else {
            self.columns
        };
        Some(DisplayEraseStep::Mutations(screen.erase_span(
            row,
            start,
            end,
            self.selective,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StyleFlags, TerminalCell};

    #[test]
    fn admission_is_profiled_and_has_no_partial_side_effects() {
        let mut screen = ScreenState::new(2, 3);
        screen.pending_wrap = true;
        for profile in [DisplayEraseProfile::Ember, DisplayEraseProfile::FrostLegacy] {
            for prefix in [None, Some(b'?'), Some(b'>'), Some(b'=')] {
                for intermediates in [b"".as_slice(), b"!", b"$", b" "] {
                    let operation =
                        prepare_display_erase(profile, &screen, &[], 'J', prefix, intermediates);
                    let admitted = profile == DisplayEraseProfile::FrostLegacy
                        || (intermediates.is_empty() && matches!(prefix, None | Some(b'?')));
                    assert_eq!(operation.is_some(), admitted);
                    assert!(screen.pending_wrap);
                }
            }
        }
        assert!(
            prepare_display_erase(DisplayEraseProfile::Ember, &screen, &[0], 'K', None, b"")
                .is_none()
        );
    }

    #[test]
    fn ember_mutates_one_row_before_each_synchronous_effect_batch() {
        let mut screen = ScreenState::new(3, 4);
        screen.grid.fill_cells(TerminalCell {
            character: 'X',
            ..TerminalCell::default()
        });
        screen.pending_wrap = true;
        let Some(DisplayErase::Rows(mut operation)) =
            prepare_display_erase(DisplayEraseProfile::Ember, &screen, &[0], 'J', None, b"")
        else {
            panic!("ED0 rows")
        };
        assert!(matches!(
            operation.step(&mut screen),
            Some(DisplayEraseStep::ScreenErased)
        ));
        assert!(screen.pending_wrap);
        assert_eq!(screen.grid.get(0, 0).character, 'X');
        for row in 0..3 {
            let Some(DisplayEraseStep::Mutations(effects)) = operation.step(&mut screen) else {
                panic!("one row batch")
            };
            assert!(effects
                .iter()
                .all(|effect| effect.row == row && effect.printed.is_none()));
            assert!(!screen.pending_wrap);
            assert_eq!(screen.grid.get(row, 0).character, ' ');
            if row + 1 < 3 {
                assert_eq!(screen.grid.get(row + 1, 0).character, 'X');
            }
        }
        assert!(operation.step(&mut screen).is_none());
    }

    #[test]
    fn selective_wrap_and_frost_dirty_order_keep_their_original_contracts() {
        let mut screen = ScreenState::new(2, 3);
        let mut protected = StyleFlags::default();
        protected.set_protected(true);
        screen.grid.fill_cells(TerminalCell {
            character: 'X',
            flags: protected,
            ..TerminalCell::default()
        });
        for mode in [0, 1, 2] {
            screen.pending_wrap = true;
            let Some(DisplayErase::Rows(mut operation)) = prepare_display_erase(
                DisplayEraseProfile::Ember,
                &screen,
                &[mode],
                'J',
                Some(b'?'),
                b"",
            ) else {
                panic!("DECSED rows")
            };
            while let Some(step) = operation.step(&mut screen) {
                assert!(matches!(step, DisplayEraseStep::Mutations(effects) if effects.is_empty()));
            }
            assert_eq!(screen.pending_wrap, mode != 0);
            assert_eq!(screen.grid.get(0, 0).character, 'X');
        }
        screen.cursor_col = 1;
        screen.pending_wrap = true;
        let Some(DisplayErase::Rows(mut operation)) = prepare_display_erase(
            DisplayEraseProfile::FrostLegacy,
            &screen,
            &[0],
            'J',
            Some(b'?'),
            b"!",
        ) else {
            panic!("legacy ED0 rows")
        };
        assert!(
            matches!(operation.step(&mut screen), Some(DisplayEraseStep::DirtyRows(range)) if range == (0..=1))
        );
        assert!(screen.pending_wrap);
        assert_eq!(screen.grid.get(1, 0).character, ' ');
        assert!(operation.step(&mut screen).is_none());
    }
}

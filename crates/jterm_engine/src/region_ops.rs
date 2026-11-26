//! Ordered native IL/DL and SU/SD transactions. Each completed physical step
//! must be consumed by the host before the next step or input event.
use crate::{
    grid::RegionDirection,
    screen::{RegionMutation, ScreenState},
    RawRowId, ScrollbackLine,
};
use std::ops::RangeInclusive;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionOpsProfile {
    Ember,
    FrostLegacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Lines,
    Scroll,
}

#[derive(Debug)]
pub enum RegionStep {
    Scroll {
        mutation: RegionMutation,
        /// Frost's original history codec captures before the physical shift.
        frost_capture: Option<ScrollbackLine>,
        transfer_outgoing: bool,
        allow_alt_scrollback: bool,
    },
    Lines {
        mutation: Option<RegionMutation>,
        /// Both native profiles notify this range even for out-of-margin IL/DL.
        dirty: RangeInclusive<usize>,
    },
}

pub struct RegionOperation {
    profile: RegionOpsProfile,
    kind: Kind,
    direction: RegionDirection,
    count: usize,
    remaining: usize,
}

/// Both existing native profiles admit these commands independently of CSI
/// namespaces/intermediates. The parser has already admitted the event.
pub fn prepare_region_operation(
    profile: RegionOpsProfile,
    screen: &ScreenState,
    params: &[u16],
    cmd: char,
) -> Option<RegionOperation> {
    let (kind, direction) = match cmd {
        'L' => (Kind::Lines, RegionDirection::Down),
        'M' => (Kind::Lines, RegionDirection::Up),
        'S' => (Kind::Scroll, RegionDirection::Up),
        'T' => (Kind::Scroll, RegionDirection::Down),
        _ => return None,
    };
    let count = params.first().copied().unwrap_or(1).max(1) as usize;
    let remaining = if kind == Kind::Lines {
        1
    } else {
        count.min(
            screen
                .scroll_region_bottom
                .saturating_sub(screen.scroll_region_top)
                .saturating_add(1),
        )
    };
    Some(RegionOperation {
        profile,
        kind,
        direction,
        count,
        remaining,
    })
}

impl RegionOperation {
    /// `allow_alt_archive` is the host's current sync-output/inline-CLI policy,
    /// recomputed after each history callback. Geometry/input stay serialized.
    /// The allocator serves shared screen shifts and Ember line edits; Frost
    /// IL/DL retains its established process-wide allocation primitive.
    pub fn step(
        &mut self,
        screen: &mut ScreenState,
        allow_alt_archive: bool,
        allocate_id: impl FnMut() -> RawRowId,
    ) -> Option<RegionStep> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let top = screen.scroll_region_top;
        let bottom = screen.scroll_region_bottom;
        if self.kind == Kind::Lines {
            let cursor = screen.cursor_row;
            let (mutation, dirty) = match self.profile {
                RegionOpsProfile::Ember => (
                    if (top..=bottom).contains(&cursor) {
                        screen.edit_lines(self.direction, cursor, bottom, self.count, allocate_id)
                    } else {
                        None
                    },
                    cursor..=bottom,
                ),
                RegionOpsProfile::FrostLegacy => (
                    None,
                    crate::semantics::frost_legacy_splice_lines(screen, self.direction, self.count),
                ),
            };
            return Some(RegionStep::Lines { mutation, dirty });
        }
        if top > bottom || bottom >= screen.grid.rows() {
            self.remaining = 0;
            return None;
        }
        let allow_alt_scrollback =
            self.direction == RegionDirection::Up && screen.use_alt_buffer && allow_alt_archive;
        let transfer_outgoing = self.direction == RegionDirection::Up
            && top == 0
            && (!screen.use_alt_buffer || allow_alt_scrollback);
        let frost_capture = (self.profile == RegionOpsProfile::FrostLegacy && transfer_outgoing)
            .then(|| screen.grid.archive_frost_legacy_row(top));
        let mutation = screen.scroll_region_one(
            self.direction,
            top,
            bottom,
            transfer_outgoing && self.profile == RegionOpsProfile::Ember,
            allocate_id,
        )?;
        Some(RegionStep::Scroll {
            mutation,
            frost_capture,
            transfer_outgoing,
            allow_alt_scrollback,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TerminalCell;

    #[test]
    fn counted_scroll_preserves_one_step_history_and_effect_boundaries() {
        for profile in [RegionOpsProfile::Ember, RegionOpsProfile::FrostLegacy] {
            let mut screen = ScreenState::new(3, 2);
            for row in 0..3 {
                screen.grid[row].fill(TerminalCell {
                    character: char::from(b'A' + row as u8),
                    ..TerminalCell::default()
                });
            }
            let mut operation = prepare_region_operation(profile, &screen, &[99], 'S').unwrap();
            for expected in ['A', 'B', 'C'] {
                let Some(RegionStep::Scroll {
                    mutation,
                    frost_capture,
                    transfer_outgoing,
                    ..
                }) = operation.step(&mut screen, false, RawRowId::allocate_process_unique)
                else {
                    panic!("one scroll")
                };
                assert!(transfer_outgoing);
                let captured = if profile == RegionOpsProfile::Ember {
                    mutation.removed[0].captured.as_ref().unwrap()
                } else {
                    frost_capture.as_ref().unwrap()
                };
                assert_eq!(captured.decompress()[0].character, expected);
            }
            assert!(operation
                .step(&mut screen, false, RawRowId::allocate_process_unique)
                .is_none());
        }
    }

    #[test]
    fn one_row_scroll_and_outside_margin_line_dirty_contracts_survive() {
        for profile in [RegionOpsProfile::Ember, RegionOpsProfile::FrostLegacy] {
            let mut screen = ScreenState::new(1, 2);
            screen.grid[0][0].character = 'X';
            let mut operation = prepare_region_operation(profile, &screen, &[0], 'T').unwrap();
            assert!(matches!(
                operation.step(&mut screen, false, RawRowId::allocate_process_unique),
                Some(RegionStep::Scroll { .. })
            ));
            assert_eq!(screen.grid[0][0].character, ' ');
            let mut screen = ScreenState::new(4, 2);
            screen.scroll_region_top = 1;
            screen.scroll_region_bottom = 2;
            let mut operation = prepare_region_operation(profile, &screen, &[5], 'L').unwrap();
            assert!(
                matches!(operation.step(&mut screen, false, RawRowId::allocate_process_unique), Some(RegionStep::Lines { mutation: None, dirty }) if dirty == (0..=2))
            );
        }
    }
}

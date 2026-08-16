//! Click-to-place-cursor: turning a mouse click into line-editor movement.
//!
//! Modern editors put the caret where you click. A terminal cannot do that
//! directly — the caret belongs to the line editor inside the shell, on the
//! far side of the PTY — so the only honest way to move it is to synthesise
//! the arrow keys the user would otherwise hold down. This module owns that
//! translation for all four frontends: when a click is allowed to move the
//! cursor at all, how far it may travel, and what bytes that becomes.
//!
//! Two rules are worth stating up front because they are what makes the
//! feature safe rather than merely convenient.
//!
//! **Rightward overshoot is not harmless.** Moving further left than the
//! buffer start is a no-op in every line editor, but `Right` at end-of-buffer
//! is a *command* in fish-style editors — in `jsh` it accepts the inline
//! autosuggestion and appends text the user never typed. So a click past the
//! end of the input must be clamped to the end of the input, never rounded up
//! to "however many cells the user clicked".
//!
//! **Cells are not characters.** Arrow keys step over characters; a click
//! lands on a cell. A wide (CJK, emoji) character occupies two cells, so
//! counting cells would send twice the moves it should. Callers that own a
//! cell grid count with [`char_steps`]; the VTE-backed frontends read the text
//! between the two points and count its characters. Either way what reaches
//! [`arrow_bytes`] is a character count.

/// A zero-based cell coordinate.
///
/// Rows live in whatever absolute row space the caller uses — VTE ring rows,
/// scrollback-plus-grid rows — and only need to be consistent between the
/// cursor, the click and the input span handed to one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub row: i64,
    pub col: i64,
}

impl Cell {
    pub fn new(row: i64, col: i64) -> Self {
        Self { row, col }
    }

    /// Row-major cell index, used to order and clamp positions.
    fn index(self, columns: i64) -> i64 {
        self.row.saturating_mul(columns).saturating_add(self.col)
    }
}

/// What the shell is doing, as far as the frontend can tell.
///
/// `Unknown` is the honest answer for a shell that emits no OSC 133, and it
/// must stay permissive: requiring positive proof of a prompt would turn the
/// feature off for plain `bash`, which is exactly where hand-walking a long
/// command hurts most. `Running` is the case worth refusing — a foreground
/// program that did not enable mouse reporting (`less`, a pager, a TUI that
/// only reads keys) would read the synthesised arrows as scrolling commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellPhase {
    Unknown,
    Editing,
    Running,
}

/// Default for the family-shared `click_moves_cursor` config key.
///
/// On by default: the whole point is that the behaviour matches what people
/// already expect from an editor, and every dangerous case is vetoed by
/// [`Guards`] rather than by making users opt in.
pub const ENABLED_BY_DEFAULT: bool = true;

/// Everything that can veto a click-to-move, gathered in one place so the four
/// frontends cannot drift apart on the conditions.
#[derive(Clone, Copy, Debug)]
pub struct Guards {
    /// The `click_moves_cursor` config key.
    pub enabled: bool,
    /// The application asked for mouse reporting (any of DECSET 1000/1002/
    /// 1003/1006). The click belongs to it, not to us.
    pub mouse_reporting: bool,
    /// The alternate screen is active, so there is no shell line editor on
    /// screen to move a cursor within.
    pub alt_screen: bool,
    /// The scrollback is scrolled away from the live grid. The clicked row is
    /// then history, not the line being edited.
    pub scrolled_back: bool,
    pub phase: ShellPhase,
}

/// The single predicate the frontends share. Kept as a function rather than
/// four inlined `&&` chains so a new veto lands everywhere at once.
pub fn click_may_move_cursor(guards: &Guards) -> bool {
    guards.enabled
        && !guards.mouse_reporting
        && !guards.alt_screen
        && !guards.scrolled_back
        && guards.phase != ShellPhase::Running
}

/// The stretch of cells the line editor currently owns.
///
/// `start` is where the shell's input begins (the cell just past the prompt);
/// `end` is one past its last character. Horizontal overshoot on either
/// boundary row is clamped to the nearer edge, so clicking the prompt means
/// "go to the beginning" and clicking the empty space after the command means
/// "go to the end". Rows outside the logical line are history or terminal
/// furniture and are refused by [`target_cell`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputSpan {
    pub start: Cell,
    pub end: Cell,
}

/// Refuse absurd journeys instead of flooding the PTY.
///
/// A run this long cannot come from a plausible command line; it comes from a
/// stale cursor position or a row space mismatch, and a wrong-but-large move
/// is worse than no move at all.
pub const MAX_STEPS: i64 = 4096;

/// Decide which cell a click should move the cursor to, or `None` when the
/// click should be left alone.
///
/// With a known `span`, clicks on one of its rows are clamped horizontally at
/// the first and last row. Rows outside the span are refused: they belong to
/// history or other terminal furniture, and interacting with them must not
/// disturb the live editor. Without a span — a frontend that cannot tell where
/// the input begins and ends — only same-row clicks are honoured, because a
/// cross-row move would have to guess how many characters separate rows it
/// knows nothing about.
pub fn target_cell(
    cursor: Cell,
    click: Cell,
    columns: i64,
    span: Option<InputSpan>,
) -> Option<Cell> {
    if columns <= 0 {
        return None;
    }

    let target = match span {
        Some(span) => {
            let (lo, hi) = if span.start.index(columns) <= span.end.index(columns) {
                (span.start, span.end)
            } else {
                (span.end, span.start)
            };
            if click.row < lo.row || click.row > hi.row {
                return None;
            }
            let clicked = click.index(columns);
            if clicked < lo.index(columns) {
                lo
            } else if clicked > hi.index(columns) {
                hi
            } else {
                click
            }
        }
        None if click.row == cursor.row => click,
        None => return None,
    };

    (target != cursor).then_some(target)
}

/// Signed character distance from `from` to `to`, positive when `to` is to the
/// right, for callers that own a cell grid.
///
/// `is_wide_continuation` reports whether a cell is the second half of a wide
/// character. Those cells carry no character of their own, so they are skipped:
/// counting them would send two `Right`s for one CJK character.
pub fn char_steps<F>(from: Cell, to: Cell, columns: i64, is_wide_continuation: F) -> i64
where
    F: Fn(i64, i64) -> bool,
{
    if columns <= 0 {
        return 0;
    }

    let forward = to.index(columns) >= from.index(columns);
    let (lo, hi) = if forward { (from, to) } else { (to, from) };

    let mut counted = 0i64;
    let mut row = lo.row;
    let mut col = lo.col;
    let hi_index = hi.index(columns);
    // Walk cells rather than subtracting indices: only the walk can see which
    // cells are wide continuations, and the cap keeps a bad row space from
    // turning into an unbounded loop.
    while Cell::new(row, col).index(columns) < hi_index && counted <= MAX_STEPS {
        if !is_wide_continuation(row, col) {
            counted += 1;
        }
        col += 1;
        if col >= columns {
            col = 0;
            row += 1;
        }
    }

    if forward {
        counted
    } else {
        -counted
    }
}

/// Press/drag/release bookkeeping for frontends whose toolkit reports a press
/// and a release but not "this was a click".
///
/// A press on the terminal is ambiguous: it may place the cursor, or it may be
/// the first frame of a selection drag. Waiting for the release and checking
/// that the pointer never left the pressed cell is what separates them, and
/// doing it here keeps the three frontends that need it from each inventing a
/// slightly different rule.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClickTracker {
    pending: Option<Cell>,
}

impl ClickTracker {
    /// Record a button press. `plain` is the caller's verdict on modifiers,
    /// button and press count — anything but an unmodified single left press
    /// belongs to selection, link opening or the application.
    pub fn press(&mut self, cell: Cell, plain: bool) {
        self.pending = plain.then_some(cell);
    }

    /// Report where the pointer is now. Leaving the pressed cell turns the
    /// gesture into a drag, which must not move the cursor.
    pub fn pointer_at(&mut self, cell: Cell) {
        if self.pending.is_some_and(|pressed| pressed != cell) {
            self.pending = None;
        }
    }

    /// Consume the gesture, yielding the clicked cell if it really was a click.
    pub fn release(&mut self) -> Option<Cell> {
        self.pending.take()
    }

    /// Drop the gesture without acting on it (focus loss, a cancelled
    /// sequence, a frontend that decided the click belongs elsewhere).
    pub fn cancel(&mut self) {
        self.pending = None;
    }
}

/// Clamp a move to what the input actually holds.
///
/// `max_left`/`max_right` are character counts available on each side of the
/// cursor. Callers that cannot measure one side pass [`MAX_STEPS`]; the left
/// side is genuinely safe to overshoot, the right side is not (see the module
/// docs).
pub fn clamp_steps(steps: i64, max_left: i64, max_right: i64) -> i64 {
    steps.clamp(-max_left.max(0), max_right.max(0))
}

/// Encode a character-distance move as arrow keys.
///
/// Returns nothing for a zero move and for anything past [`MAX_STEPS`].
/// `application_cursor_keys` is DECCKM: an editor that put the terminal in
/// application mode expects `SS3` and would render `CSI` arrows as literal
/// text.
pub fn arrow_bytes(steps: i64, application_cursor_keys: bool) -> Vec<u8> {
    if steps == 0 || steps.saturating_abs() > MAX_STEPS {
        return Vec::new();
    }

    let (right, left): (&[u8], &[u8]) = if application_cursor_keys {
        (b"\x1bOC", b"\x1bOD")
    } else {
        (b"\x1b[C", b"\x1b[D")
    };
    let sequence = if steps > 0 { right } else { left };

    let count = steps.unsigned_abs() as usize;
    let mut out = Vec::with_capacity(count * sequence.len());
    for _ in 0..count {
        out.extend_from_slice(sequence);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: i64 = 80;

    fn no_wide(_row: i64, _col: i64) -> bool {
        false
    }

    fn guards() -> Guards {
        Guards {
            enabled: true,
            mouse_reporting: false,
            alt_screen: false,
            scrolled_back: false,
            phase: ShellPhase::Unknown,
        }
    }

    #[test]
    fn a_plain_prompt_click_is_allowed() {
        assert!(click_may_move_cursor(&guards()));
    }

    #[test]
    fn every_guard_can_veto_on_its_own() {
        for veto in [
            Guards {
                enabled: false,
                ..guards()
            },
            Guards {
                mouse_reporting: true,
                ..guards()
            },
            Guards {
                alt_screen: true,
                ..guards()
            },
            Guards {
                scrolled_back: true,
                ..guards()
            },
            Guards {
                phase: ShellPhase::Running,
                ..guards()
            },
        ] {
            assert!(!click_may_move_cursor(&veto), "{veto:?} should veto");
        }
    }

    #[test]
    fn an_unknown_shell_phase_stays_permissive() {
        // Plain bash emits no OSC 133; refusing there would remove the feature
        // from the shell that needs it most.
        assert!(click_may_move_cursor(&Guards {
            phase: ShellPhase::Unknown,
            ..guards()
        }));
        assert!(click_may_move_cursor(&Guards {
            phase: ShellPhase::Editing,
            ..guards()
        }));
    }

    #[test]
    fn without_a_span_only_the_cursor_row_moves() {
        let cursor = Cell::new(10, 20);
        assert_eq!(
            target_cell(cursor, Cell::new(10, 5), COLS, None),
            Some(Cell::new(10, 5))
        );
        assert_eq!(target_cell(cursor, Cell::new(9, 5), COLS, None), None);
        assert_eq!(target_cell(cursor, cursor, COLS, None), None);
    }

    #[test]
    fn a_span_clamps_on_its_boundary_rows() {
        let span = InputSpan {
            start: Cell::new(10, 2),
            end: Cell::new(11, 7),
        };
        let cursor = Cell::new(10, 40);

        // Clicking the prompt means "go to the beginning".
        assert_eq!(
            target_cell(cursor, Cell::new(10, 0), COLS, Some(span)),
            Some(span.start)
        );
        // Clicking past the command — blank cells, or jsh's inline suggestion —
        // means "go to the end", never "walk off it".
        assert_eq!(
            target_cell(cursor, Cell::new(11, 60), COLS, Some(span)),
            Some(span.end)
        );
        // Inside, the click is taken as-is, across a soft wrap.
        assert_eq!(
            target_cell(cursor, Cell::new(11, 3), COLS, Some(span)),
            Some(Cell::new(11, 3))
        );
    }

    #[test]
    fn rows_outside_the_input_span_preserve_the_cursor() {
        let span = InputSpan {
            start: Cell::new(10, 2),
            end: Cell::new(11, 7),
        };
        let cursor = span.end;

        assert_eq!(
            target_cell(cursor, Cell::new(9, 40), COLS, Some(span)),
            None,
            "a click in a historical block must not become Home"
        );
        assert_eq!(
            target_cell(cursor, Cell::new(12, 0), COLS, Some(span)),
            None,
            "terminal furniture below the editor must not become End"
        );
    }

    #[test]
    fn a_reversed_span_is_ordered_before_clamping() {
        let span = InputSpan {
            start: Cell::new(11, 7),
            end: Cell::new(10, 2),
        };
        assert_eq!(
            target_cell(Cell::new(10, 4), Cell::new(10, 0), COLS, Some(span)),
            Some(Cell::new(10, 2))
        );
    }

    #[test]
    fn a_click_on_the_cursor_moves_nothing() {
        let span = InputSpan {
            start: Cell::new(10, 2),
            end: Cell::new(10, 20),
        };
        let cursor = Cell::new(10, 9);
        assert_eq!(target_cell(cursor, cursor, COLS, Some(span)), None);
        // Clamping onto the cursor is a no-op too: the span ends where the
        // cursor already is.
        let at_end = InputSpan {
            start: Cell::new(10, 2),
            end: cursor,
        };
        assert_eq!(
            target_cell(cursor, Cell::new(10, 40), COLS, Some(at_end)),
            None
        );
    }

    #[test]
    fn steps_count_characters_not_cells() {
        // Two CJK characters at columns 4 and 6; their continuations sit at 5
        // and 7 and must not be counted.
        let wide = |_row: i64, col: i64| col == 5 || col == 7;
        assert_eq!(
            char_steps(Cell::new(0, 4), Cell::new(0, 8), COLS, wide),
            2,
            "four cells of CJK are two Right presses"
        );
        assert_eq!(char_steps(Cell::new(0, 8), Cell::new(0, 4), COLS, wide), -2);
        assert_eq!(
            char_steps(Cell::new(0, 4), Cell::new(0, 8), COLS, no_wide),
            4
        );
    }

    #[test]
    fn steps_cross_a_soft_wrap() {
        assert_eq!(
            char_steps(Cell::new(3, 78), Cell::new(4, 2), COLS, no_wide),
            4
        );
        assert_eq!(
            char_steps(Cell::new(4, 2), Cell::new(3, 78), COLS, no_wide),
            -4
        );
    }

    #[test]
    fn a_degenerate_grid_never_loops() {
        assert_eq!(char_steps(Cell::new(0, 0), Cell::new(9, 9), 0, no_wide), 0);
        assert_eq!(target_cell(Cell::new(0, 0), Cell::new(0, 5), 0, None), None);
    }

    #[test]
    fn clamping_protects_the_right_edge_only() {
        // Left overshoot is a no-op in every line editor, so a caller that
        // cannot measure the left side loses nothing.
        assert_eq!(clamp_steps(-500, MAX_STEPS, 3), -500);
        // Right overshoot would accept jsh's autosuggestion.
        assert_eq!(clamp_steps(500, MAX_STEPS, 3), 3);
        assert_eq!(clamp_steps(7, 2, 0), 0);
        assert_eq!(clamp_steps(-7, 0, 9), 0);
        assert_eq!(
            clamp_steps(-7, -1, 9),
            0,
            "a negative bound is treated as 0"
        );
    }

    #[test]
    fn a_tracker_only_yields_a_click_that_never_moved() {
        let mut tracker = ClickTracker::default();

        tracker.press(Cell::new(4, 9), true);
        tracker.pointer_at(Cell::new(4, 9));
        assert_eq!(tracker.release(), Some(Cell::new(4, 9)));
        assert_eq!(tracker.release(), None, "a gesture is consumed once");

        // Leaving the cell makes it a selection drag.
        tracker.press(Cell::new(4, 9), true);
        tracker.pointer_at(Cell::new(4, 10));
        tracker.pointer_at(Cell::new(4, 9));
        assert_eq!(tracker.release(), None);

        // Modified, non-left or multi-click presses never arm it.
        tracker.press(Cell::new(4, 9), false);
        assert_eq!(tracker.release(), None);

        tracker.press(Cell::new(4, 9), true);
        tracker.cancel();
        assert_eq!(tracker.release(), None);

        // Motion with nothing pressed is not a gesture.
        tracker.pointer_at(Cell::new(1, 1));
        assert_eq!(tracker.release(), None);
    }

    #[test]
    fn arrows_encode_direction_and_decckm() {
        assert_eq!(arrow_bytes(2, false), b"\x1b[C\x1b[C".to_vec());
        assert_eq!(arrow_bytes(-2, false), b"\x1b[D\x1b[D".to_vec());
        assert_eq!(arrow_bytes(2, true), b"\x1bOC\x1bOC".to_vec());
        assert_eq!(arrow_bytes(-1, true), b"\x1bOD".to_vec());
        assert!(arrow_bytes(0, false).is_empty());
    }

    #[test]
    fn an_implausible_distance_sends_nothing() {
        assert!(arrow_bytes(MAX_STEPS + 1, false).is_empty());
        assert!(arrow_bytes(-(MAX_STEPS + 1), false).is_empty());
        assert_eq!(arrow_bytes(MAX_STEPS, false).len() as i64, MAX_STEPS * 3);
    }

    #[test]
    fn the_whole_path_holds_together_for_a_wrapped_command() {
        // An 80-column grid, a two-row command starting at row 10 column 2 and
        // ending at row 11 column 7, cursor parked at the end. The user clicks
        // the blank space well past the command on the second row.
        let columns = COLS;
        let span = InputSpan {
            start: Cell::new(10, 2),
            end: Cell::new(11, 7),
        };
        let cursor = span.end;
        let click = Cell::new(11, 50);

        assert_eq!(
            target_cell(cursor, click, columns, Some(span)),
            None,
            "clamped back onto the cursor, so nothing is sent"
        );

        // And a click in the middle of the first row walks left across the wrap.
        let click = Cell::new(10, 12);
        let target = target_cell(cursor, click, columns, Some(span)).unwrap();
        let steps = char_steps(cursor, target, columns, no_wide);
        assert_eq!(steps, -(80 - 12 + 7));
        let clamped = clamp_steps(steps, MAX_STEPS, 0);
        assert_eq!(arrow_bytes(clamped, false).len() as i64, -steps * 3);
    }
}

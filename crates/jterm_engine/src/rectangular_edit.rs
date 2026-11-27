//! DEC rectangular fill and erasure, completed one physical row at a time.
use crate::{
    grid::{RectangularCopyScratch, RectangularRendition},
    screen::{RowMutation, ScreenState},
    Color, HyperlinkId, TerminalCell,
};
use smallvec::SmallVec;

pub(crate) struct RectangularEdit {
    next_row: usize,
    end_row: usize,
    left: usize,
    right: usize,
    operation: Operation,
}

enum Operation {
    Erase,
    Selective,
    Attributes {
        rectangle_extent: bool,
        first_row: usize,
        rendition: RectangularRendition,
    },
    Copy(RectangularCopy),
    Fill {
        cell: TerminalCell,
        underline: Color,
    },
}

struct RectangularCopy {
    source_top: usize,
    target_top: usize,
    target_left: usize,
    reverse: bool,
    scratch: RectangularCopyScratch,
}

/// Admit only exact DEC rectangular edit namespaces. Invalid rectangles are
/// consumed as empty operations, so they cannot reach legacy CSI fallbacks.
pub(crate) fn prepare_rectangular_edit(
    screen: &ScreenState,
    params: &[u16],
    cmd: char,
    prefix: Option<u8>,
    intermediates: &[u8],
) -> Option<RectangularEdit> {
    if prefix.is_some() || intermediates != b"$" || !matches!(cmd, 'z' | '{' | 'x' | 'v') {
        return None;
    }
    let rows = screen.grid.rows();
    let cols = screen.grid.cols();
    let mut result = RectangularEdit {
        next_row: 0,
        end_row: 0,
        left: 0,
        right: 0,
        operation: if cmd == '{' {
            Operation::Selective
        } else {
            Operation::Erase
        },
    };
    if rows == 0 || cols == 0 {
        return Some(result);
    }
    let params = if cmd == 'x' {
        if params.len() > 5 {
            return Some(result);
        }
        let value = params.first().copied().filter(|&v| v != 0).unwrap_or(32);
        if !matches!(value, 32..=126 | 160..=255) {
            return Some(result);
        }
        result.operation = Operation::Fill {
            cell: TerminalCell {
                character: screen.translate_fill_character(value as u8),
                foreground: screen.current_fg,
                background: screen.current_bg,
                flags: screen.current_flags,
                hyperlink_id: HyperlinkId::NONE,
            },
            underline: Color::Default,
        };
        params.get(1..).unwrap_or_default()
    } else {
        let max_parameters = if cmd == 'v' { 8 } else { 4 };
        if params.len() > max_parameters {
            return Some(result);
        }
        params
    };
    let (origin, page_bottom) = if screen.origin_mode {
        (
            screen.scroll_region_top.min(rows - 1),
            screen.scroll_region_bottom.min(rows - 1),
        )
    } else {
        (0, rows - 1)
    };
    if origin > page_bottom {
        return Some(result);
    }
    let coordinate = |index: usize, default: usize, offset: usize, maximum: usize| {
        let value = params
            .get(index)
            .copied()
            .filter(|&v| v != 0)
            .map(usize::from)
            .unwrap_or(default);
        offset.saturating_add(value - 1).min(maximum)
    };
    let top = coordinate(0, 1, origin, page_bottom);
    let left = coordinate(1, 1, 0, cols - 1);
    let bottom = coordinate(2, page_bottom - origin + 1, origin, page_bottom);
    let right = coordinate(3, cols, 0, cols - 1);
    if top <= bottom && left <= right {
        if cmd == 'v' {
            // The engine exposes one page; both page parameters normalize to it.
            let target_top = coordinate(5, 1, origin, page_bottom);
            let target_left = coordinate(6, 1, 0, cols - 1);
            let height = (bottom - top + 1).min(page_bottom - target_top + 1);
            let width = (right - left + 1).min(cols - target_left);
            // Admission is atomic even if a retained target annotation makes a
            // later row exceed its rich-text budget.
            if (0..height).any(|offset| {
                !screen.grid.rectangular_copy_fits(
                    top + offset,
                    left..left + width,
                    target_top + offset,
                    target_left,
                )
            }) {
                return Some(result);
            }
            result.next_row = 0;
            result.end_row = height;
            result.left = left;
            result.right = left + width;
            result.operation = Operation::Copy(RectangularCopy {
                source_top: top,
                target_top,
                target_left,
                reverse: target_top > top,
                scratch: RectangularCopyScratch::default(),
            });
            return Some(result);
        }
        result.next_row = top;
        result.end_row = bottom + 1;
        result.left = left;
        result.right = right + 1;
    }
    Some(result)
}

/// DEC attribute edits use stream extent by default. Only visual rendition
/// changes; character contents, cell metadata and the live pen are retained.
pub(crate) fn prepare_rectangular_attributes(
    screen: &ScreenState,
    params: &[u16],
    cmd: char,
    prefix: Option<u8>,
    intermediates: &[u8],
    rectangle_extent: bool,
) -> Option<RectangularEdit> {
    if prefix.is_some() || intermediates != b"$" || !matches!(cmd, 'r' | 't') {
        return None;
    }
    let mut edit =
        prepare_rectangular_edit(screen, &params[..params.len().min(4)], 'z', None, b"$")?;
    edit.operation = Operation::Attributes {
        rectangle_extent,
        first_row: edit.next_row,
        rendition: RectangularRendition::compile(params.get(4..).unwrap_or_default(), cmd == 't'),
    };
    Some(edit)
}

/// XTerm XTREPORTSGR reports common stored cell rendition, including blank
/// cells. It neither moves the cursor nor expands clipped wide glyphs.
pub(crate) fn report_rectangular_sgr(
    screen: &ScreenState,
    params: &[u16],
    cmd: char,
    prefix: Option<u8>,
    intermediates: &[u8],
) -> Option<Vec<u8>> {
    use crate::{StyleFlags, UnderlineStyle};
    use std::fmt::Write;
    if cmd != '|' || prefix.is_some() || intermediates != b"#" || params.len() > 4 {
        return None;
    }
    let rectangle = prepare_rectangular_edit(screen, params, 'z', None, b"$")?;
    let mut common: Option<(StyleFlags, [Color; 3])> = None;
    for row in rectangle.next_row..rectangle.end_row {
        let underlines = screen.grid.underline_colors(row);
        for col in rectangle.left..rectangle.right {
            let cell = screen.grid.get(row, col);
            let colors = [
                canonical_color(cell.foreground),
                canonical_color(cell.background),
                canonical_color(underlines.get(col).copied().unwrap_or(Color::Default)),
            ];
            let (flags, common_colors) = common.get_or_insert((cell.flags, colors));
            *flags = flags.common_sgr(cell.flags);
            for (common, color) in common_colors.iter_mut().zip(colors) {
                if *common != color {
                    *common = Color::Default;
                }
            }
            if *flags == StyleFlags::default() && *common_colors == [Color::Default; 3] {
                return Some(b"\x1b[0m".to_vec());
            }
        }
    }
    let mut reply = String::from("\x1b[0");
    if let Some((flags, [fg, bg, ul])) = common {
        if flags.bold() {
            reply.push_str(";1");
        }
        let underline = flags.underline();
        match underline {
            UnderlineStyle::Single => reply.push_str(";4"),
            UnderlineStyle::Curly => reply.push_str(";4:3"),
            UnderlineStyle::Dotted => reply.push_str(";4:4"),
            UnderlineStyle::Dashed => reply.push_str(";4:5"),
            UnderlineStyle::None | UnderlineStyle::Double => {}
        }
        for (enabled, value) in [
            (flags.blink(), 5),
            (flags.inverse(), 7),
            (flags.concealed(), 8),
            (flags.dim(), 2),
            (flags.italic(), 3),
            (flags.strikethrough(), 9),
        ] {
            if enabled {
                let _ = write!(reply, ";{value}");
            }
        }
        if underline == UnderlineStyle::Double {
            reply.push_str(";21");
        }
        if flags.overline() {
            reply.push_str(";53");
        }
        append_report_color(&mut reply, fg, 38);
        append_report_color(&mut reply, bg, 48);
        append_report_color(&mut reply, ul, 58);
    }
    reply.push('m');
    Some(reply.into_bytes())
}

fn canonical_color(color: Color) -> Color {
    use Color::*;
    match color {
        Black => Indexed(0),
        Red => Indexed(1),
        Green => Indexed(2),
        Yellow => Indexed(3),
        Blue => Indexed(4),
        Magenta => Indexed(5),
        Cyan => Indexed(6),
        White => Indexed(7),
        BrightBlack => Indexed(8),
        BrightRed => Indexed(9),
        BrightGreen => Indexed(10),
        BrightYellow => Indexed(11),
        BrightBlue => Indexed(12),
        BrightMagenta => Indexed(13),
        BrightCyan => Indexed(14),
        BrightWhite => Indexed(15),
        _ => color,
    }
}
fn append_report_color(reply: &mut String, color: Color, plane: u8) {
    use std::fmt::Write;
    match color {
        Color::Indexed(index) if plane != 58 && index < 16 => {
            let base = if plane == 38 { 30 } else { 40 };
            let value = if index < 8 {
                base + index
            } else {
                base + 60 + index - 8
            };
            let _ = write!(reply, ";{value}");
        }
        Color::Indexed(index) => {
            let _ = write!(reply, ";{plane}:5:{index}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(reply, ";{plane}:2::{r}:{g}:{b}");
        }
        _ => {}
    }
}

impl RectangularEdit {
    pub(crate) fn set_fill_underline_color(&mut self, color: Color) {
        if let Operation::Fill { underline, .. } = &mut self.operation {
            *underline = color;
        }
    }
    pub(crate) fn step(&mut self, screen: &mut ScreenState) -> Option<SmallVec<[RowMutation; 4]>> {
        if self.next_row >= self.end_row {
            return None;
        }
        if let Operation::Attributes {
            rectangle_extent,
            first_row,
            rendition,
        } = &self.operation
        {
            let row = self.next_row;
            self.next_row += 1;
            let left = if *rectangle_extent || row == *first_row {
                self.left
            } else {
                0
            };
            let right = if *rectangle_extent || self.next_row == self.end_row {
                self.right
            } else {
                screen.grid.cols()
            };
            let mutations = screen
                .grid
                .apply_rectangular_rendition(row, left..right, *rectangle_extent, *rendition)
                .map(|changed| RowMutation {
                    row,
                    row_id: screen.grid.row_id(row),
                    changed,
                    printed: None,
                });
            return Some(mutations.into_iter().collect());
        }
        if let Operation::Copy(copy) = &mut self.operation {
            let offset = if copy.reverse {
                self.end_row - self.next_row - 1
            } else {
                self.next_row
            };
            self.next_row += 1;
            let row = copy.target_top + offset;
            let changed = screen.grid.copy_rectangular_row(
                copy.source_top + offset,
                self.left..self.right,
                row,
                copy.target_left,
                &mut copy.scratch,
            );
            return Some(smallvec::smallvec![RowMutation {
                row,
                row_id: screen.grid.row_id(row),
                changed,
                printed: None
            }]);
        }
        let row = self.next_row;
        self.next_row += 1;
        let row_id = screen.grid.row_id(row);
        if !matches!(self.operation, Operation::Selective) {
            let (start, end) = screen.expanded_cell_mutation_span(row, self.left, self.right);
            match &self.operation {
                Operation::Erase => {
                    screen
                        .grid
                        .erase_cells(row, self.left..self.right, TerminalCell::default())
                }
                Operation::Fill { cell, underline } => {
                    screen
                        .grid
                        .fill_rectangular_span(row, self.left..self.right, *cell, *underline)
                }
                Operation::Selective | Operation::Copy(_) | Operation::Attributes { .. } => {
                    unreachable!()
                }
            }
            return Some(smallvec::smallvec![RowMutation {
                row,
                row_id,
                changed: start..end,
                printed: None,
            }]);
        }
        let mut mutations: SmallVec<[RowMutation; 4]> = SmallVec::new();
        let cols = screen.grid.cols();
        let mut col = self.left;
        while col < self.right {
            let start = if col > 0 && screen.grid.get(row, col).flags.wide_continuation() {
                col - 1
            } else {
                col
            };
            let end = if screen.grid.get(row, start).flags.wide() && start + 1 < cols {
                start + 2
            } else {
                col + 1
            };
            let protected = (start..end).any(|c| screen.grid.get(row, c).flags.protected());
            if !protected {
                if let Some(previous) = mutations
                    .last_mut()
                    .filter(|previous| previous.changed.end == start)
                {
                    previous.changed.end = end;
                } else {
                    mutations.push(RowMutation {
                        row,
                        row_id,
                        changed: start..end,
                        printed: None,
                    });
                }
            }
            col = end;
        }
        for mutation in &mutations {
            let affected = &mutation.changed;
            // Content annotations are erased as one run. The separate
            // underline rendition stays attached to the physical cells.
            for cell in screen.grid.row_cells_mut(row, affected.clone()) {
                cell.character = ' ';
                cell.hyperlink_id = HyperlinkId::NONE;
                cell.flags.clear_cell_contents();
            }
            screen.grid.erase_annotations(row, affected.clone());
        }
        Some(mutations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Color, StyleFlags};
    fn erase(screen: &mut ScreenState, params: &[u16], cmd: char) -> Vec<RowMutation> {
        let mut operation = prepare_rectangular_edit(screen, params, cmd, None, b"$").unwrap();
        let mut effects = Vec::new();
        while let Some(row) = operation.step(screen) {
            effects.extend(row);
        }
        effects
    }
    #[test]
    fn geometry_defaults_origin_clamping_and_strict_admission() {
        let mut screen = ScreenState::new(5, 7);
        screen.grid.fill_cells(TerminalCell {
            character: 'X',
            ..TerminalCell::default()
        });
        screen.scroll_region_top = 1;
        screen.scroll_region_bottom = 3;
        screen.origin_mode = true;
        let effects = erase(&mut screen, &[0, 2, 99, 99], 'z');
        assert_eq!(
            effects
                .iter()
                .map(|m| (m.row, m.changed.clone()))
                .collect::<Vec<_>>(),
            vec![(1, 1..7), (2, 1..7), (3, 1..7)]
        );
        assert_eq!(screen.grid.get(0, 2).character, 'X');
        assert_eq!(screen.grid.get(4, 2).character, 'X');
        assert!(erase(&mut screen, &[3, 4, 1, 2], 'z').is_empty());
        assert!(erase(&mut screen, &[1, 1, 2, 2, 1], 'z').is_empty());
        assert!(prepare_rectangular_edit(&screen, &[], 'z', Some(b'?'), b"$").is_none());
        assert!(prepare_rectangular_edit(&screen, &[], 'z', None, b" $").is_none());
        screen.origin_mode = false;
        assert_eq!(erase(&mut screen, &[], 'z').len(), 5);
    }
    #[test]
    fn selective_keeps_rendition_and_pair_protection_and_clears_content() {
        let mut screen = ScreenState::new(2, 6);
        let mut flags = StyleFlags::default();
        flags.set_bold(true);
        screen.grid.fill_cells(TerminalCell {
            character: 'X',
            foreground: Color::Red,
            background: Color::Blue,
            flags,
            ..TerminalCell::default()
        });
        screen.grid.get_mut(0, 1).flags.set_wide(true);
        screen.grid.get_mut(0, 2).flags.set_wide_continuation(true);
        screen.grid.get_mut(0, 2).flags.set_protected(true);
        screen.grid.set_underline_color(0, 0..6, Color::Green);
        screen.cursor_row = 1;
        screen.cursor_col = 5;
        screen.pending_wrap = true;
        screen.grid.row_wrapped[0] = true;
        let id = screen.grid.row_id(0);
        let effects = erase(&mut screen, &[1, 2, 1, 4], '{');
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].changed, 3..4);
        assert_eq!(screen.grid.get(0, 1).character, 'X');
        assert_eq!(screen.grid.get(0, 2).character, 'X');
        let blank = screen.grid.get(0, 3);
        assert_eq!(blank.character, ' ');
        assert!(blank.flags.bold());
        assert_eq!(blank.foreground, Color::Red);
        assert_eq!(screen.grid.underline_colors(0)[3], Color::Green);
        assert_eq!(
            (screen.cursor_row, screen.cursor_col, screen.pending_wrap),
            (1, 5, true)
        );
        assert!(screen.grid.row_wrapped[0]);
        assert_eq!(screen.grid.row_id(0), id);
        assert!(effects.iter().all(|m| m.printed.is_none()));
        let effects = erase(&mut screen, &[1, 3, 1, 3], 'z');
        assert_eq!(effects[0].changed, 1..3);
        assert_eq!(*screen.grid.get(0, 1), TerminalCell::default());
        assert_eq!(*screen.grid.get(0, 2), TerminalCell::default());
        assert_eq!(screen.grid.underline_colors(0)[2], Color::Default);
        assert!(screen.pending_wrap);
    }
    #[test]
    fn selective_wide_edge_repairs_are_complete_and_unstyled_erase_is_default() {
        let mut screen = ScreenState::new(1, 5);
        screen.grid.get_mut(0, 1).flags.set_wide(true);
        screen.grid.get_mut(0, 2).flags.set_wide_continuation(true);
        let effects = erase(&mut screen, &[1, 3, 1, 3], '{');
        assert_eq!(effects[0].changed, 1..3);
        assert!(!screen.grid.get(0, 1).flags.wide());
        assert!(!screen.grid.get(0, 2).flags.wide_continuation());
        screen.current_bg = Color::Red;
        screen.current_flags.set_bold(true);
        erase(&mut screen, &[], 'z');
        assert_eq!(*screen.grid.get(0, 4), TerminalCell::default());
    }

    #[test]
    fn selective_erase_clears_copy_provenance_from_overwritten_tab_spaces() {
        let mut screen = ScreenState::new(1, 6);
        screen.grid.track_printed_spaces = true;
        screen.grid.mark_tab_spaces(0, 0..4);
        erase(&mut screen, &[1, 2, 1, 3], '{');
        for col in 0..4 {
            let flags = screen.grid.get(0, col).flags;
            assert_eq!(flags.tab_padding(), matches!(col, 0 | 3));
            assert_eq!(flags.printed_space(), matches!(col, 0 | 3));
        }
    }
    #[test]
    fn fill_defaults_ranges_gl_and_latin1_are_explicit() {
        let mut screen = ScreenState::new(2, 5);
        for params in [&[][..], &[0][..], &[32][..]] {
            screen.grid.fill_cells(TerminalCell {
                character: 'X',
                ..TerminalCell::default()
            });
            erase(&mut screen, params, 'x');
            assert_eq!(screen.grid.get(1, 4).character, ' ');
        }
        for value in [1, 31, 127, 128, 159, 256, u16::MAX] {
            screen.grid.fill_cells(TerminalCell {
                character: 'X',
                ..TerminalCell::default()
            });
            assert!(erase(&mut screen, &[value], 'x').is_empty());
            assert_eq!(screen.grid.get(1, 4).character, 'X');
        }
        screen.active_charset = crate::screen::Charset::DecSpecialGraphics;
        erase(&mut screen, &[113, 1, 1, 1, 2], 'x');
        assert_eq!(screen.grid.get(0, 0).character, '─');
        assert_eq!(screen.grid.get(0, 1).character, '─');
        erase(&mut screen, &[163, 1, 3, 1, 3], 'x');
        assert_eq!(screen.grid.get(0, 2).character, '£');
        erase(&mut screen, &[255, 1, 4, 1, 4], 'x');
        assert_eq!(screen.grid.get(0, 3).character, 'ÿ');
        assert!(erase(&mut screen, &[65, 1, 1, 2, 5, 1], 'x').is_empty());
    }

    #[test]
    fn fill_rendition_underline_and_wide_collateral_preserve_cursor_state() {
        let mut screen = ScreenState::new(2, 6);
        screen.grid.track_printed_spaces = true;
        screen.grid.mark_tab_spaces(0, 0..6);
        screen.grid.get_mut(0, 1).flags.set_wide(true);
        screen.grid.get_mut(0, 2).flags.set_wide_continuation(true);
        screen.grid.get_mut(0, 3).flags.set_wide(true);
        screen.grid.get_mut(0, 4).flags.set_wide_continuation(true);
        screen.grid.get_mut(0, 2).flags.set_protected(true);
        screen.current_fg = Color::Red;
        screen.current_bg = Color::Blue;
        screen.current_flags.set_bold(true);
        screen.current_flags.set_protected(true);
        screen.current_hyperlink = HyperlinkId::from_raw(4);
        screen.grid.set_underline_color(0, 0..6, Color::Green);
        screen.cursor_row = 1;
        screen.cursor_col = 5;
        screen.pending_wrap = true;
        screen.grid.row_wrapped[0] = true;
        let id = screen.grid.row_id(0);
        let mut fill =
            prepare_rectangular_edit(&screen, &[32, 1, 3, 1, 4], 'x', None, b"$").unwrap();
        fill.set_fill_underline_color(Color::Red);
        let effects = fill.step(&mut screen).unwrap();
        assert_eq!(effects[0].changed, 1..5);
        assert_eq!(effects[0].printed, None);
        assert!(fill.step(&mut screen).is_none());
        for col in [1, 4] {
            assert_eq!(*screen.grid.get(0, col), TerminalCell::default());
        }
        for col in 2..4 {
            let cell = screen.grid.get(0, col);
            assert_eq!(cell.character, ' ');
            assert!(cell.flags.bold() && cell.flags.protected() && cell.flags.printed_space());
            assert!(
                !cell.flags.tab_padding() && !cell.flags.wide() && !cell.flags.wide_continuation()
            );
            assert_eq!(
                (cell.foreground, cell.background, cell.hyperlink_id),
                (Color::Red, Color::Blue, HyperlinkId::NONE)
            );
            assert_eq!(screen.grid.underline_colors(0)[col], Color::Red);
        }
        assert_eq!(screen.grid.underline_colors(0)[0], Color::Green);
        assert_eq!(screen.grid.underline_colors(0)[1], Color::Default);
        assert_eq!(screen.grid.underline_colors(0)[4], Color::Default);
        assert_eq!(
            (screen.cursor_row, screen.cursor_col, screen.pending_wrap),
            (1, 5, true)
        );
        assert_eq!(screen.grid.row_id(0), id);
        assert!(screen.grid.row_wrapped[0]);
    }
    #[test]
    fn copy_overlap_matches_independent_snapshot_for_small_rectangles() {
        let (rows, cols) = (3, 4);
        for top in 0..rows {
            for bottom in top..rows {
                for left in 0..cols {
                    for right in left..cols {
                        for target_top in 0..rows {
                            for target_left in 0..cols {
                                let mut screen = ScreenState::new(rows, cols);
                                let mut before = Vec::new();
                                for row in 0..rows {
                                    for col in 0..cols {
                                        let mut flags = StyleFlags::default();
                                        flags.set_bold((row + col) % 2 == 0);
                                        flags.set_protected(row == 1);
                                        let cell = TerminalCell {
                                            character: char::from(b'A' + (row * cols + col) as u8),
                                            foreground: Color::Red,
                                            background: Color::Blue,
                                            flags,
                                            hyperlink_id: HyperlinkId::from_raw((col + 1) as u16),
                                        };
                                        *screen.grid.get_mut(row, col) = cell;
                                        before.push(cell);
                                    }
                                    screen.grid.row_wrapped[row] = row % 2 == 0;
                                }
                                screen.cursor_row = 2;
                                screen.cursor_col = 3;
                                screen.pending_wrap = true;
                                let ids: Vec<_> =
                                    (0..rows).map(|r| screen.grid.row_id(r)).collect();
                                let mut expected = before.clone();
                                let height = (bottom - top + 1).min(rows - target_top);
                                let width = (right - left + 1).min(cols - target_left);
                                for y in 0..height {
                                    for x in 0..width {
                                        expected[(target_top + y) * cols + target_left + x] =
                                            before[(top + y) * cols + left + x];
                                    }
                                }
                                let params = [
                                    (top + 1) as u16,
                                    (left + 1) as u16,
                                    (bottom + 1) as u16,
                                    (right + 1) as u16,
                                    7,
                                    (target_top + 1) as u16,
                                    (target_left + 1) as u16,
                                    9,
                                ];
                                let effects = erase(&mut screen, &params, 'v');
                                let actual: Vec<_> = (0..rows)
                                    .flat_map(|r| (0..cols).map(move |c| (r, c)))
                                    .map(|(r, c)| *screen.grid.get(r, c))
                                    .collect();
                                assert_eq!(actual,expected,"source={top},{left}..{bottom},{right} target={target_top},{target_left}");
                                let expected_order: Vec<_> = if target_top > top {
                                    (target_top..target_top + height).rev().collect()
                                } else {
                                    (target_top..target_top + height).collect()
                                };
                                assert_eq!(
                                    effects.iter().map(|m| m.row).collect::<Vec<_>>(),
                                    expected_order
                                );
                                assert!(effects.iter().all(|m| m.printed.is_none()));
                                assert_eq!(
                                    (screen.cursor_row, screen.cursor_col, screen.pending_wrap),
                                    (2, 3, true)
                                );
                                assert_eq!(
                                    (0..rows).map(|r| screen.grid.row_id(r)).collect::<Vec<_>>(),
                                    ids
                                );
                                assert_eq!(screen.grid.row_wrapped, [true, false, true]);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn copy_defaults_origin_and_clipped_destination_use_shared_geometry() {
        let mut screen = ScreenState::new(5, 6);
        screen.scroll_region_top = 1;
        screen.scroll_region_bottom = 3;
        screen.origin_mode = true;
        for row in 0..5 {
            for col in 0..6 {
                screen.grid.get_mut(row, col).character = char::from(b'A' + row as u8);
            }
        }
        let effects = erase(&mut screen, &[1, 1, 0, 0, 0, 3, 6, 65535], 'v');
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].row, 3);
        assert_eq!(effects[0].changed, 5..6);
        assert_eq!(screen.grid.get(3, 5).character, 'B');
        assert_eq!(screen.grid.get(4, 5).character, 'E');
        assert!(erase(&mut screen, &[3, 1, 1, 2], 'v').is_empty());
        assert!(erase(&mut screen, &[1, 1, 2, 2, 1, 1, 1, 1, 1], 'v').is_empty());
        let before: Vec<_> = (0..5)
            .flat_map(|r| (0..6).map(move |c| (r, c)))
            .map(|(r, c)| *screen.grid.get(r, c))
            .collect();
        erase(&mut screen, &[], 'v');
        let after: Vec<_> = (0..5)
            .flat_map(|r| (0..6).map(move |c| (r, c)))
            .map(|(r, c)| *screen.grid.get(r, c))
            .collect();
        assert_eq!(before, after);
    }
    fn report(screen: &ScreenState, params: &[u16]) -> Vec<u8> {
        report_rectangular_sgr(screen, params, '|', None, b"#").unwrap()
    }
    #[test]
    fn report_sgr_intersects_cells_and_normalizes_named_indexed_colors() {
        let mut screen = ScreenState::new(2, 3);
        let mut flags = StyleFlags::default();
        flags.set_bold(true);
        flags.set_italic(true);
        flags.set_underline(crate::UnderlineStyle::Curly);
        screen.grid.fill_cells(TerminalCell {
            character: 'X',
            foreground: Color::Red,
            background: Color::Rgb(2, 3, 4),
            flags,
            ..TerminalCell::default()
        });
        screen
            .grid
            .set_underline_color(0, 0..3, Color::Indexed(200));
        screen
            .grid
            .set_underline_color(1, 0..3, Color::Indexed(200));
        screen.grid.get_mut(0, 1).foreground = Color::Indexed(1);
        screen
            .grid
            .set_underline_color(0, 1..2, Color::Indexed(200));
        assert_eq!(
            report(&screen, &[]),
            b"\x1b[0;1;4:3;3;31;48:2::2:3:4;58:5:200m"
        );
        screen.grid.get_mut(1, 2).flags.set_bold(false);
        screen.grid.get_mut(1, 2).background = Color::Indexed(4);
        assert_eq!(report(&screen, &[]), b"\x1b[0;4:3;3;31m");
        // Default cells participate, irrespective of drawn/blank occupancy.
        *screen.grid.get_mut(1, 2) = TerminalCell::default();
        assert_eq!(report(&screen, &[]), b"\x1b[0m");
    }
    #[test]
    fn report_sgr_order_extensions_and_exact_wide_boundaries_are_read_only() {
        let mut screen = ScreenState::new(1, 3);
        let mut flags = StyleFlags::default();
        flags.set_bold(true);
        flags.set_dim(true);
        flags.set_italic(true);
        flags.set_blink(true);
        flags.set_inverse(true);
        flags.set_concealed(true);
        flags.set_strikethrough(true);
        flags.set_overline(true);
        flags.set_protected(true);
        flags.set_underline(crate::UnderlineStyle::Double);
        screen.grid.fill_cells(TerminalCell {
            character: 'X',
            foreground: Color::Indexed(12),
            background: Color::Indexed(240),
            flags,
            hyperlink_id: HyperlinkId::from_raw(2),
        });
        screen.grid.get_mut(0, 0).flags.set_wide(true);
        screen.grid.get_mut(0, 1).flags.set_wide_continuation(true);
        screen.grid.get_mut(0, 1).flags.set_bold(false);
        screen.cursor_col = 2;
        screen.pending_wrap = true;
        screen.grid.row_wrapped[0] = true;
        screen.grid.begin_capture_revision(9);
        screen.grid.begin_capture_revision(10);
        let before: Vec<_> = (0..3).map(|c| *screen.grid.get(0, c)).collect();
        assert_eq!(
            report(&screen, &[1, 1, 1, 1]),
            b"\x1b[0;1;5;7;8;2;3;9;21;53;94;48:5:240m"
        );
        assert_eq!(
            report(&screen, &[1, 2, 1, 2]),
            b"\x1b[0;5;7;8;2;3;9;21;53;94;48:5:240m"
        );
        assert_eq!(
            (0..3).map(|c| *screen.grid.get(0, c)).collect::<Vec<_>>(),
            before
        );
        assert!(!screen.grid.row_changed_after(0, 9));
        assert_eq!((screen.cursor_col, screen.pending_wrap), (2, true));
        assert!(screen.grid.row_wrapped[0]);
    }
    #[test]
    fn report_sgr_geometry_admission_defaults_and_rgb_indexed_distinction() {
        let mut screen = ScreenState::new(4, 4);
        screen.scroll_region_top = 1;
        screen.scroll_region_bottom = 2;
        screen.origin_mode = true;
        for row in 1..3 {
            for col in 0..4 {
                screen.grid.get_mut(row, col).flags.set_bold(true);
            }
        }
        assert_eq!(report(&screen, &[0, 0, 99, 99]), b"\x1b[0;1m");
        assert_eq!(report(&screen, &[2, 1, 1, 4]), b"\x1b[0m");
        assert_eq!(report(&screen, &[1, 4, 2, 1]), b"\x1b[0m");
        assert!(report_rectangular_sgr(&screen, &[1, 1, 1, 1, 1], '|', None, b"#").is_none());
        assert!(report_rectangular_sgr(&screen, &[], '|', Some(b'?'), b"#").is_none());
        assert!(report_rectangular_sgr(&screen, &[], '|', None, b" #").is_none());
        screen.grid.get_mut(1, 0).foreground = Color::Rgb(255, 0, 0);
        screen.grid.get_mut(1, 1).foreground = Color::Indexed(1);
        assert_eq!(report(&screen, &[1, 1, 1, 2]), b"\x1b[0;1m");
    }
    fn attributes(
        screen: &mut ScreenState,
        params: &[u16],
        cmd: char,
        rectangle: bool,
    ) -> Vec<RowMutation> {
        let mut operation =
            prepare_rectangular_attributes(screen, params, cmd, None, b"$", rectangle).unwrap();
        let mut effects = Vec::new();
        while let Some(row) = operation.step(screen) {
            effects.extend(row);
        }
        effects
    }

    #[test]
    fn attribute_stream_wraps_page_width_and_skips_undrawn_cells() {
        let mut screen = ScreenState::new(3, 6);
        for row in 0..3 {
            screen.grid.fill_rectangular_span(
                row,
                1..5,
                TerminalCell {
                    character: 'X',
                    ..TerminalCell::default()
                },
                Color::Default,
            );
        }
        screen.scroll_region_top = 1;
        screen.scroll_region_bottom = 2;
        let effects = attributes(&mut screen, &[1, 3, 3, 4, 1], 'r', false);
        assert_eq!(
            effects
                .iter()
                .map(|m| (m.row, m.changed.clone()))
                .collect::<Vec<_>>(),
            vec![(0, 2..5), (1, 1..5), (2, 1..4)]
        );
        for row in 0..3 {
            for col in 0..6 {
                let expected = (1..5).contains(&col)
                    && match row {
                        0 => col >= 2,
                        1 => true,
                        _ => col < 4,
                    };
                assert_eq!(screen.grid.get(row, col).flags.bold(), expected);
            }
        }
        assert!(!screen.grid.cell_drawn(1, 0));
        assert!(!screen.grid.cell_drawn(1, 5));
        // Source order validation is rectangular even for stream extent.
        assert!(attributes(&mut screen, &[1, 5, 3, 2, 7], 'r', false).is_empty());
    }

    #[test]
    fn rectangle_attributes_materialize_blanks_and_preserve_all_nonvisual_state() {
        let mut screen = ScreenState::new(2, 5);
        screen.grid.get_mut(0, 1).flags.set_wide(true);
        screen.grid.get_mut(0, 2).flags.set_wide_continuation(true);
        screen.grid.get_mut(0, 2).flags.set_protected(true);
        screen.grid.get_mut(0, 2).hyperlink_id = HyperlinkId::from_raw(7);
        screen.grid.set_underline_color(0, 1..3, Color::Red);
        screen.cursor_row = 1;
        screen.cursor_col = 4;
        screen.pending_wrap = true;
        screen.current_flags.set_italic(true);
        screen.grid.row_wrapped[0] = true;
        let ids: Vec<_> = (0..2).map(|r| screen.grid.row_id(r)).collect();
        let effects = attributes(&mut screen, &[1, 3, 2, 4, 1, 4, 5, 7], 'r', true);
        assert_eq!(
            effects
                .iter()
                .map(|m| (m.row, m.changed.clone()))
                .collect::<Vec<_>>(),
            vec![(0, 2..4), (1, 2..4)]
        );
        assert!(!screen.grid.get(0, 1).flags.bold());
        let cell = screen.grid.get(0, 2);
        assert!(
            cell.flags.bold()
                && cell.flags.blink()
                && cell.flags.inverse()
                && cell.flags.protected()
                && cell.flags.wide_continuation()
        );
        assert_eq!(cell.flags.underline(), crate::UnderlineStyle::Single);
        assert_eq!(cell.hyperlink_id, HyperlinkId::from_raw(7));
        assert_eq!(screen.grid.underline_colors(0)[2], Color::Red);
        assert!(screen.grid.cell_drawn(1, 3));
        assert_eq!(screen.grid.get(1, 3).character, ' ');
        assert_eq!(
            (screen.cursor_row, screen.cursor_col, screen.pending_wrap),
            (1, 4, true)
        );
        assert!(screen.current_flags.italic() && !screen.current_flags.bold());
        assert!(screen.grid.row_wrapped[0]);
        assert_eq!(
            (0..2).map(|r| screen.grid.row_id(r)).collect::<Vec<_>>(),
            ids
        );
        assert!(effects.iter().all(|m| m.printed.is_none()));
    }

    #[test]
    fn attribute_lists_default_reset_and_reverse_follow_standard_dec_bits() {
        let mut screen = ScreenState::new(1, 2);
        let mut flags = StyleFlags::default();
        flags.set_bold(true);
        flags.set_italic(true);
        flags.set_dim(true);
        flags.set_strikethrough(true);
        flags.set_concealed(true);
        flags.set_underline(crate::UnderlineStyle::Double);
        screen.grid.fill_rectangular_span(
            0,
            0..2,
            TerminalCell {
                character: 'X',
                flags,
                ..TerminalCell::default()
            },
            Color::Default,
        );
        attributes(&mut screen, &[1, 1, 1, 2, 4, 4, 1, 1], 't', true);
        assert_eq!(
            screen.grid.get(0, 0).flags.underline(),
            crate::UnderlineStyle::Double
        );
        assert!(screen.grid.get(0, 0).flags.bold());
        attributes(&mut screen, &[], 'r', false);
        let flags = screen.grid.get(0, 0).flags;
        assert!(!flags.bold());
        assert_eq!(flags.underline(), crate::UnderlineStyle::None);
        assert!(flags.italic() && flags.dim() && flags.strikethrough() && flags.concealed());
        attributes(&mut screen, &[1, 1, 1, 2, 0], 't', true);
        let flags = screen.grid.get(0, 0).flags;
        assert!(flags.bold() && flags.blink() && flags.inverse());
        assert_eq!(flags.underline(), crate::UnderlineStyle::Single);
        attributes(&mut screen, &[1, 1, 1, 2, 22, 24, 25, 27, 8, 9], 'r', true);
        let flags = screen.grid.get(0, 0).flags;
        assert!(!flags.bold() && !flags.blink() && !flags.inverse());
        assert!(flags.strikethrough() && flags.concealed());
        assert!(
            prepare_rectangular_attributes(&screen, &[], 'r', Some(b'?'), b"$", true).is_none()
        );
        assert!(prepare_rectangular_attributes(&screen, &[], 'r', None, b" $", true).is_none());
    }
}

from pathlib import Path


def replace(path, old, new):
    p = Path(path)
    s = p.read_text()
    if s.count(old) != 1:
        raise SystemExit(f'{path}: ambiguous refinement: {old[:80]!r}, count={s.count(old)}')
    p.write_text(s.replace(old, new))


p = 'runtime/vim9/bridge.vim'
replace(p, "var renderLineGeneration = '' \n", "var renderLineGeneration = ''\n")
replace(p, "      && (lineNumber != line('.') || byte + 1 != col('.'))", "      && previous.target !=# [lineNumber, byte]")
replace(p, '      tick: b:changedtick, mode: mode(1), cursor: getcurpos()[1 : 2]}',
        '      tick: b:changedtick, mode: mode(1), cursor: getcurpos()[1 : 2], target: [lineNumber, byte]}')
# Extend source-hit testing to explicit visible empty lines and end-of-line
# whitespace. A missing or horizontally clipped source end is never guessed.
p = Path('src/vim9_viewport.rs')
s = p.read_text()
a = s.index('    fn empty_anchor_at(')
b = s.index('    /// Re-resolve a delayed card/hover', a)
s = s[:a] + '''    fn source_rows(&self) -> impl Iterator<Item = (i64, u64, usize)> + '_ {
        self.window.line_lengths.iter().enumerate()
            .map(|(row, length)| (row as i64, self.window.topline + row as u64, *length))
            .chain(self.window.overscan.iter().map(|row| (i64::from(row.row), row.line, row.byte_length)))
    }

    fn end_anchor(&self, row: i64, line: u64, length: usize) -> Option<Vim9ViewportAnchor> {
        let mut rect = self.row_rect(row,
            self.window.view.column.checked_add(self.window.textoff)?, 1)?;
        if length > 0 {
            rect.x = self.runs().filter(|p| p.run.line == line && p.run.byte_end == length
                && p.run.byte_start < p.run.byte_end)
                .map(|p| p.rect.x + p.rect.width).reduce(f32::max)?;
        }
        Some(Vim9ViewportAnchor {
            revision: self.frame.revision,
            position: Vim9BufferPosition { window: self.window.window, buffer: self.window.buffer,
                changedtick: self.window.changedtick, line, byte_column: length },
            byte_end: length, rect, clip: intersect(rect, self.clip)?,
        })
    }

    fn empty_anchor_at(&self, x: f32, y: f32) -> Option<Vim9ViewportAnchor> {
        self.source_rows().find_map(|(row, line, length)| {
            let anchor = self.end_anchor(row, line, length)?;
            (x >= anchor.rect.x && y >= anchor.clip.y
                && y < anchor.clip.y + anchor.clip.height).then_some(anchor)
        })
    }

''' + s[b:]
a = s.index('            .or_else(|| {', s.index('    pub fn anchor_for_position'))
b = s.index('\n    }\n}\n\n#[cfg(test)]', a)
s = s[:a] + '''            .or_else(|| {
                self.source_rows().find_map(|(row, line, length)| {
                    if line == position.line && length == position.byte_column {
                        self.end_anchor(row, line, length)
                    } else { None }
                })
            })''' + s[b:]
# Add tests inside the existing module so its fixture is reused.
s = s.rstrip()[:-1] + '''
    #[test]
    fn explicit_visible_empty_rows_and_eol_are_source_targets() {
        let mut frame = frame();
        frame.windows[0].line_lengths = vec![2, 3, 1, 0];
        let view = Vim9Viewport::new(&frame, 10, metrics(), -0.5).unwrap();
        let empty = view.anchor_at(45.0, 65.0).unwrap();
        assert_eq!((empty.position.line, empty.position.byte_column), (13, 0));
        assert_eq!(view.anchor_for_position(&empty.position), Some(empty));
        let end = view.anchor_at(45.0, 5.0).unwrap();
        assert_eq!((end.position.line, end.position.byte_column), (10, 2));
        assert_eq!(view.anchor_for_position(&end.position), Some(end));
        assert!(view.anchor_at(65.0, 65.0).is_none());
    }

    #[test]
    fn absent_visible_source_metadata_does_not_invent_an_empty_line() {
        let frame = frame();
        let view = Vim9Viewport::new(&frame, 10, metrics(), 0.0).unwrap();
        assert!(view.anchor_at(45.0, 65.0).is_none());
    }
}
'''
p.write_text(s)
p = 'runtime/vim9/test_pixel_input.py'
replace(p, '    def test_drag_cannot_cross_into_another_split(self):', '''    def test_releasing_at_the_same_eol_does_not_accidentally_start_visual(self):
        source = self.source()
        length = len('031 界 é'.encode())
        self.pointer('press', 31, length, source=source)
        self.pointer('release', 31, length, source=source)
        self.assertEqual(self.editor.snapshot()['mode'], 'n')

    def test_drag_cannot_cross_into_another_split(self):''')
print('Added explicit EOL/empty targets and same-target release regression')

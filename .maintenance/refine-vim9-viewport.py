from pathlib import Path


def replace(path, old, new):
    file = Path(path)
    text = file.read_text()
    if text.count(old) != 1:
        raise SystemExit(f'{path}: ambiguous refinement: {old[:100]!r}')
    file.write_text(text.replace(old, new))


replace('src/vim9.rs', '["render_protocol"],\n            2', '["render_protocol"],\n            3')
replace('src/vim9_viewport.rs', '''            self.anchor(projected, offset)
        })
    }
}''', '''            self.anchor(projected, offset)
        }).or_else(|| {
            if position.byte_column != 0 {
                return None;
            }
            let row = self.window.overscan.iter()
                .find(|row| row.line == position.line && row.byte_length == 0)?;
            let rect = self.row_rect(i64::from(row.row),
                self.window.view.column.checked_add(self.window.textoff)?, 1)?;
            Some(Vim9ViewportAnchor {
                revision: self.frame.revision, position: *position, byte_end: 0,
                rect, clip: intersect(rect, self.clip)?,
            })
        })
    }
}''')
replace('src/vim9_viewport.rs', '''        assert_eq!((hit.position.line, hit.position.byte_column, hit.byte_end), (15, 0, 0));''', '''        assert_eq!((hit.position.line, hit.position.byte_column, hit.byte_end), (15, 0, 0));
        assert_eq!(view.anchor_for_position(&hit.position), Some(hit));''')
replace('runtime/vim9/test_overscan.py',
    'def test_text_properties_syntax_search_and_signs_are_not_plain_text_prefetch(self):',
    'def test_text_properties_keep_gui_styling_in_hidden_rows(self):')
# Vim accepts an empty range only when end == start - 1. Bottom overscan
# can otherwise put the first filler row several positions past the end.
replace('runtime/vim9/bridge.vim',
    'range(view.row + last - first + 1, view.row + view.rows + visibleFirst - first - 1)',
    'range(view.row + last - first + 1, max([view.row + last - first, view.row + view.rows + visibleFirst - first - 1]))')
print('Updated v3 negotiation, empty-row anchors and bounded EOB fill ranges')

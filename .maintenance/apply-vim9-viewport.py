from pathlib import Path
import re


def replace(path, old, new):
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f'{path}: expected one patch location, got {count}: {old[:80]!r}')
    file.write_text(text.replace(old, new))


p = 'src/vim9_render.rs'
replace(p, '/// A visible Vim window, sampled in that window\'s own buffer/option context.', '''/// A real buffer row outside the visible split. `row` is signed and relative
/// to its viewport's top, never a terminal screen coordinate. Nested runs use
/// row zero; their columns and source bytes retain the same meaning as visible
/// runs. Empty lines are explicit, so missing paint is never guessed as text.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Vim9RenderMarginRow {
    pub row: i32,
    pub line: u64,
    pub byte_length: usize,
    pub runs: Vec<Vim9RenderRun>,
}

/// A visible Vim window, sampled in that window's own buffer/option context.''')
replace(p, '    pub textoff: usize,\n}', '''    pub textoff: usize,
    /// Zero for old wire versions. Otherwise the actual buffer line count.
    pub line_count: u64,
    /// Version 3: at most two independently sampled source rows on each edge.
    pub overscan: Vec<Vim9RenderMarginRow>,
}''')
replace(p, '            && self.runs.len() <= 24_000', '''            && self.runs.len() + self.windows.iter().flat_map(|window| &window.overscan)
                .map(|row| row.runs.len()).sum::<usize>() <= 24_000''')
replace(p, '                    && window.textoff <= window.view.columns', '''                    && window.textoff <= window.view.columns
                    && (window.line_count == 0 || (window.topline > 0
                        && window.topline <= window.botline && window.botline <= window.line_count))
                    && window.overscan.len() <= 4
                    && window.overscan.iter().enumerate().all(|(index, row)| {
                        let source_line = if row.row < 0 {
                            window.topline.checked_sub(u64::from(row.row.unsigned_abs()))
                        } else {
                            window.topline.checked_add(row.row as u64)
                        };
                        window.line_count > 0 && row.line > 0 && row.line <= window.line_count
                            && source_line == Some(row.line)
                            && ((-2..0).contains(&row.row)
                                || (window.view.rows as i32..window.view.rows as i32 + 2).contains(&row.row))
                            && row.byte_length <= 16_384 && row.runs.len() <= 512
                            && window.overscan[..index].iter().all(|other| other.row < row.row)
                            && row.runs.iter().all(|run| {
                                run.window == window.window && run.row == 0 && run.cells > 0
                                    && run.column >= window.view.column
                                    && within(run.column, run.cells, window.view.column + window.view.columns)
                                    && (run.line == 0 || run.line == row.line)
                                    && run.byte_start <= run.byte_end
                                    && (run.line == 0 || run.byte_end <= row.byte_length)
                                    && run.text.len() <= 64 * 1024
                                    && !run.text.chars().any(|character| character.is_ascii_control())
                                    && style_valid(&run.style)
                            })
                    })''')

p = 'src/vim9.rs'
replace(p, '    Vim9RenderWindow,\n};', '    Vim9RenderWindow, Vim9RenderMarginRow,\n};')
replace(p, 'const PROTOCOL: u64 = 1;', '''#[path = "vim9_viewport.rs"]
mod viewport;
pub use viewport::{Vim9ProjectedRun, Vim9Viewport, Vim9ViewportAnchor};

const PROTOCOL: u64 = 1;''')
text = Path(p).read_text()
text, count = re.subn(r'("render_protocol"\s*:\s*)2\b', r'\g<1>3', text)
if count != 1:
    raise SystemExit(f'Expected one native render protocol negotiation, found {count}')
Path(p).write_text(text)

p = 'runtime/vim9/bridge.vim'
replace(p, 'def RenderWindowFrame(): dict<any>', 'def RenderWindowFrame(margin: number = 0): dict<any>')
replace(p, '  frame.textoff = info.textoff\n', '''  frame.textoff = info.textoff
  frame.line_count = line('$')
  frame.overscan = []
  var visibleFirst = first
  if margin > 0
    first = max([1, first - margin])
    last = min([line('$'), last + margin])
  endif
''')
replace(p, 'last - first > 200', 'last - first > 200 + 2 * margin')
replace(p, 'range(view.row + last - first + 1, view.row + view.rows - 1)', 'range(view.row + last - first + 1, view.row + view.rows + visibleFirst - first - 1)')
replace(p, '  var position = screenpos(win_getid(), line(\'.\'), col(\'.\'))', '''  if margin > 0
    var marginRows: dict<dict<any>> = {}
    for lineNumber in range(first, last)
      var relative = lineNumber - visibleFirst
      if relative < 0 || relative >= view.rows
        var edge = {row: relative, line: lineNumber, byte_length: strlen(getline(lineNumber)), runs: []}
        marginRows[string(relative)] = edge
        add(frame.overscan, edge)
      endif
    endfor
    var visibleRuns: list<dict<any>> = []
    for run in frame.runs
      var relative = run.row - view.row - (visibleFirst - first)
      if relative >= 0 && relative < view.rows
        run.row = view.row + relative
        add(visibleRuns, run)
      elseif has_key(marginRows, string(relative))
        run.row = 0
        add(marginRows[string(relative)].runs, run)
      endif
    endfor
    frame.runs = visibleRuns
  endif
  var position = screenpos(win_getid(), line('.'), col('.'))''')
replace(p, '    renderWindowSample = RenderWindowFrame()', '''    renderWindowSample = RenderWindowFrame(renderProtocol >= 3 ? 2 : 0)
    # An unsupported hidden row must not disable a supported visible viewport.
    # Retry without margins; never downgrade a real visible failure silently.
    if renderProtocol >= 3 && !renderWindowSample.supported
      renderBaseKey = ''
      renderBase = []
      renderWindowSample = RenderWindowFrame()
    endif''')
replace(p, '    for run in renderWindowSample.runs\n      run.window = win_getid()\n    endfor', '''    for run in renderWindowSample.runs
      run.window = win_getid()
    endfor
    for edge in get(renderWindowSample, 'overscan', [])
      for run in edge.runs
        run.window = win_getid()
      endfor
    endfor''')
replace(p, '      textoff: sample.textoff, leftcol: sample.leftcol})', '''      textoff: sample.textoff, leftcol: sample.leftcol,
      line_count: sample.line_count, overscan: sample.overscan})''')
replace(p, '  frame.runs = runs\n', '''  frame.runs = runs
  remove(frame, 'overscan')
''')
replace(p, '    if strlen(encoded) > 900000\n      frame.supported = false', '''    if strlen(encoded) > 900000
      for window in get(frame, 'windows', [])
        window.overscan = []
      endfor
      encoded = json_encode(frame)
    endif
    if strlen(encoded) > 900000
      frame.supported = false''')
replace(p, 'index([1, 2], requestedRender)', 'index([1, 2, 3], requestedRender)')

p = '.github/workflows/ci.yml'
replace(p, '          python3 runtime/vim9/test_scroll.py', '''          python3 runtime/vim9/test_scroll.py
          python3 runtime/vim9/test_overscan.py''')

Path('docs/vim9-pixel-viewport.md').write_text('''# Semantic margins and a single pixel viewport transform

Render protocol 3 adds up to two real buffer rows above and below each supported
split. It retains source byte ranges, GUI syntax/search/property styling, signs,
line numbers and explicit empty rows. The producer does not move Vim's window,
cursor, focus, mode, jumplist or buffer to sample these rows. Old protocols 1/2
remain accepted and do not produce margin rows.

Margins are bounded separately and are not added to the terminal-owned screen
row coordinates. Each edge row has a signed viewport-relative row index. Its
nested runs use row zero. The validator rejects out-of-range, duplicated,
unsorted, cross-window or inconsistent source rows. Margins are replaced with
window metadata in both keyframes and deltas, never silently inherited after a
new source generation. Unsupported hidden text disables margins only; an actual
unsupported visible state still uses the compatibility renderer. A size budget
first drops margins before giving up the visible native frame.

`Vim9Viewport` projects visible and margin runs into one clipped logical-pixel
space. Paint geometry, source-byte hit testing, anchor re-resolution and caret /
IME rectangles share this transform. Raw and clipped rectangles remain separate:
a half-visible glyph must be clipped, not stretched. Negative top rows work even
when the split begins at screen row zero. A transform with insufficient sampled
coverage fails closed rather than extrapolating document contents.

## Validation

Run the regular Rust suite and `python3 runtime/vim9/test_overscan.py`. The latter
launches actual Vim in a PTY with protocol 3. Legacy bridge tests retain protocol
1; delta tests retain protocol 2. Regression cases cover source identity, byte
mapping, split clipping, document edges, empty rows, property styles, hidden
unsupported text, source edits and fresh delta metadata.

## Deliberately not claimed

This is the tested producer/projection foundation, not complete pixel-direct
input in all four applications. Frontends still have their acknowledged-row
animation until their wheel admission, paint and mouse dispatch are migrated as
one transaction. Do not connect fractional input to painting alone: a click
must address the displayed source after asynchronous Vim acknowledgement, and
editing/selection, zoom, splits and fallback must cancel stale gestures safely.
No physical Wayland, hardware-GPU, Chinese-IME or VS Code latency result is
implied by these tests. Optional IDE chrome remains disabled by default.
''')
print('Applied semantic margin and viewport patches')

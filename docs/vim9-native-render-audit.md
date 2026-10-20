# Vim9 native buffer rendering: architecture audit

Audit date: 2026-10-20. The editor remains a real Vim 9 process. This document
separates the source of editing truth, the GUI paint contract, compatibility
behavior, and what was actually measured.

## Baseline verified from current repositories

The latest code had already progressed beyond the terminal-only description
in the original task. The first semantic GUI paint path was present before
this change; replacing it with a second text editor would lose working Vim
semantics and would not solve the remaining problems.

| Repository baseline | Buffer display already present | Remaining issue addressed here |
| --- | --- | --- |
| `jterm_core` `a5f055be8672de13f5070955418d1d2d84a4b868` | `runtime/vim9/bridge.vim` exports Vim-origin UTF-8 text, GUI RGB styles, byte ranges and screen geometry; `src/vim9_render.rs` validates paint data. | One-window contract, complete-frame transmission, and stale syntax cache after rules change without a buffer edit. |
| `anvil` `0b19e15ef3525da88b17c583c59aac8f74309367` | GTK/Pango/Cairo buffer painter in `src/vim9_ide_buffer.rs`; VTE retains input and compatibility output. | Paint all supported Vim split rectangles and respect native frame invalidation. |
| `forge` `9852b44c639c1e84e8cbc9a9e01de9f037865ae6` | GTK/Pango/Cairo buffer painter in `src/vim9_ide_buffer.rs`; same real Vim input contract. | Same shared multi-window contract. |
| `ember` `2c4f5074df23210d66980268573a90dc68eaf080` | Native GPU buffer painter in `src/vim9_buffer.rs` consumes shared semantic runs. | Same shared multi-window contract and explicit compatibility geometry. |
| `frost` `42a68107e27e3a6672c95b947c500eecdcee270a` | Iced/WGPU buffer painter in `src/vim9_buffer.rs` consumes shared semantic runs. | Same shared multi-window contract. |

The terminal process and its screen remain necessary for input, command rows,
status lines and unsupported Vim features. Supported buffer rectangles are
opaque GUI paint surfaces. Their text and colors are not extracted from ANSI
escape sequences. Cell positions are retained because Vim still owns mouse,
cursor, tab and wide-character geometry; this release does not claim arbitrary
proportional reflow of Vim's editing windows.

## What stock Vim exposes

The public Vim APIs below are evaluated inside the actual editor; the host
does not infer buffer contents from terminal output.

| Information | Public API | Contract consequence |
| --- | --- | --- |
| Buffer text and revision | `getline()`, `getbufline()`, `b:changedtick` | UTF-8 content and byte positions originate in Vim. |
| Buffer edits | `listener_add()`, `listener_flush()` | Buffer changes are observable; these are not complete display changes. |
| Syntax and GUI attributes | `synID()`, `synIDtrans()`, `synIDattr(..., 'gui')`, `hlget()` | Read syntax and linked RGB/style definitions independently of a terminal palette. |
| Windows and viewports | `getwininfo()`, `winlayout()`, `winsaveview()` | Preserve Vim-owned split IDs and visible rectangles. |
| Cursor and Visual positions | `getpos()`, `virtcol()`, `screenpos()` | Keep byte indexes distinct from display columns. |
| Final screen characters | `screenstring()`, `screenchars()` | Useful as test oracles, including composing characters. |
| Final screen attributes | `screenattr()` | Opaque comparison IDs, not a public RGB/style decoder. |
| Extra decorations | `prop_list()`, `prop_type_get()`, `sign_getplaced()`, `getmatches()` | Each supported decoration needs explicit composition or compatibility mode. |

Source: [Vim's builtin function reference](https://raw.githubusercontent.com/vim/vim/master/runtime/doc/builtin.txt).
Syntax and highlight semantics are described in
[Vim's syntax reference](https://vimhelp.org/syntax.txt.html), and properties in
[Vim's text property reference](https://vimhelp.org/textprop.txt.html).

There is no assumption of Neovim's `nvim_ui_attach` or its redraw event
protocol. A final `screenattr()` value cannot safely be passed to `synIDattr()`
as if it were a highlight-group ID. The local probe returned final screen
attribute IDs such as 2096/2098 while the syntax group ID was 73; querying the
former as syntax groups produced empty names.

Vim 9.2's current reference also documents `redraw_listener_add()` with
`on_start`/`on_end` callbacks and an `unbuffered` listener option. These are
redraw scheduling hooks, with no complete glyph/style event payload. The test
runtime here is Vim 9.1 and does not expose them; a future adapter must check
`exists('*redraw_listener_add')` instead of assuming every Vim 9 has this API.

## Display model and incremental protocol

Vim remains the only editing engine. Keyboard, committed IME text, mouse and
save commands reach that process. The paint model is read-only and contains
no independent editable buffer or undo stack.

Each complete frame has a monotonically increasing `revision`, PTY `rows` and
`columns`, the active `window`/`buffer`/`changedtick`/`mode`, a GUI default
style and the active cursor. `windows` describes every regular window in the
active Vim tab: stable `window`, `buffer`, `changedtick`, `view`, `cursor`,
`topline`, `botline`, `leftcol` and `textoff`. The top-level `view` is the
active window's rectangle for compatibility with existing metadata.

Runs use global screen row/column coordinates and identify their owning
`window`. Each carries UTF-8 text, Vim display-cell advance, original buffer
line/byte range and resolved GUI styling. The renderer paints each window's
own opaque rectangle; separators and status rows are not accidentally erased
by filling the bounding box of the whole split layout.

The negotiated `render_protocol: 2` adds `render_delta`:

- `base_revision` identifies the exact complete frame to update.
- Top-level metadata replaces the frame metadata (there is no separate
  `header` object).
- Each `row_updates` item replaces all runs on one absolute screen row,
  including runs from every side-by-side Vim window on that row.
- An empty row clears old paint. Unchanged rows remain in the host model.
- Layout/theme changes, explicit resynchronization and compatibility changes
  use complete frames. A missing or invalid base clears the native frame and
  requests `render_resync`; it never applies a delta to guessed state.

Older peers that do not negotiate version 2 continue receiving complete
`render` messages. Transport deltas reduce payload volume. They are not a
claim that Vim extraction or frontend drawing has become constant-time.

## Sampling multiple real Vim windows safely

The active window is sampled directly. Other windows use `win_execute()` with
a `noautocmd` invocation of a script-local, read-only sampler. This obtains
the target window's syntax, options, buffer and viewport without activating
it through normal window navigation. `win_execute()` itself does not issue
window-enter autocmds or change the working directory; commands executed
inside it can, which is why this callback is deliberately restricted to
reading and the invocation suppresses autocommands.

The observed behavior on Vim 9.1 is useful and tested: while the caller stays
in Visual mode, an inactive sampled window temporarily reports Normal mode.
After sampling, the caller's Visual mode and selection are intact. Inactive
windows never receive an active cursor or the caller's Visual selection.
Before/after PTY probes compare active window, buffer, cursor, view, selection
anchor, unnamed register, jumplist, current directory and enter/leave event
counters.

Base paint caches are per window, including independent viewport coordinates
when two windows show the same buffer. Caches are bounded to the active tab.
A second cache retains only each window's visible semantic lines. During an
overlapping scroll, surviving lines reuse their syntax/property runs and
copy them to their new absolute row; only newly visible or changed lines
repeat syntax extraction. A generation key includes buffer revision, theme,
search, syntax-related options and horizontal geometry. Every `changedtick`
change invalidates the whole generation because a multiline syntax region
can change arbitrarily far beyond the edited line. Per-line property changes
also invalidate that line. Line numbers, signs, cursor and selection overlays
are composed from current Vim state rather than frozen in this cache.
A layout with more than 16 windows or a window beyond the configured paint
budget uses compatibility mode. If any window needs compatibility rendering,
the entire frame does, avoiding a misleading mix of supported and unsupported
semantics. View/focus changes observed during sampling also invalidate the
result instead of publishing a mixed snapshot.

## Syntax invalidation and plugin contract

An actual defect was reproduced in the previous implementation:

```vim
syntax match GuiCache /original/
highlight GuiCache guifg=#12ab34
" Render once, then change only the rule:
syntax clear GuiCache
syntax match GuiCache /second/
```

Neither `b:changedtick`, `&syntax` nor `hlget()` changed. A cache keyed only
by those values continued painting `original` indefinitely. Command-line
mode transitions now invalidate every window's semantic base cache, and
`Syntax`/`FileType` events invalidate it as well. Returning from an unsupported
state rebuilds that window's base paint.

Stock Vim does not supply a universal event for an asynchronous plugin that
changes `:syntax` rules without changing buffer text or options. Such a
provider must explicitly notify the bridge after changing its rules:

```vim
doautocmd <nomodeline> User JTermVim9RenderInvalidate
```

The hook is read-only with respect to editing and schedules a fresh semantic
frame. A real timer-driven plugin test covers it. The bridge does not scan
all syntax definitions every 750 ms; its existing low-rate reconciliation
checks metadata, theme attributes and ordinary display state. See
[Vim's autocommand reference](https://vimhelp.org/autocmd.txt.html) for event
semantics.

## Compatibility boundaries and acceptance cases

| Area | Current behavior and test obligation |
| --- | --- |
| Real editing and saving | Typed Unicode reaches Vim; save writes Vim's actual buffer. No text-widget shadow state. |
| CJK, UTF-8 byte indexes, combining characters, simple emoji | Semantic runs preserve original bytes and Vim-owned advances. Frontend font fallback and clipping require real-window inspection. |
| IME | Preedit remains a frontend responsibility; commit goes through the existing real Vim input route. Candidate placement must follow the active window cursor. PTY Unicode tests alone do not certify an OS IME. |
| Font scale and HiDPI | GUI font rasterization is frontend-owned; the resized PTY geometry must be acknowledged before drawing old geometry. A transport test is not a HiDPI visual test. |
| Vim splits | Same/different buffers and independent scrolling are supported within paint budgets; only the active window has the active cursor/selection. |
| Visual and search | Existing bounded Visual/search composition remains active; block selections through tabs and unsupported regex forms use compatibility mode. |
| Cursor styling | Frontends draw the active cursor from Vim's position and mode. Full `guicursor` shape/blink settings and custom Cursor highlight composition are not yet exported. |
| Themes | GUI RGB attributes, links, style changes and named colors are separate from cterm palette entries. Per-window `wincolor` currently uses compatibility mode. |
| Plugins | Supported sign/text-property decorations are explicit. Popups, completion menus, match decorations and unsupported virtual text use compatibility mode. Async syntax providers use the invalidation hook above. |
| Complex layout/text | Wrapped continuation rows, closed folds, conceal, diff, RTL/bidi/control text and special Vim buffers use compatibility mode. Their options are not silently disabled. |
| Large files | Only visible bounded windows are exported. Vim's own regex syntax synchronization can still be expensive; limits on exported runs are not a bound on arbitrary plugin execution time. |

Compatibility mode exposes the real Vim screen and the reason for falling
back. It does not retain stale native text over a popup or silently claim a
feature was rendered correctly. Full plugin-equivalent composition would
require additional native decoration adapters or a maintained Vim-side GUI
draw API; this release does not invent that API.

## Font and paint quality fixes

Native RGB pixels alone are insufficient evidence of readable text. Actual
window inspection found two font defects during this implementation:

- GTK centered each fallback font independently, placing Latin and CJK on
  different baselines. The native painter now uses one baseline per row while
  retaining Vim-owned cell advances, and invalidates shaped layouts when the
  monitor scale changes.
- When the configured Nerd Font was absent, GPU font lookup could accept an
  unrelated proportional fallback. Its bold glyphs overlapped neighboring
  Vim columns. Ember now checks the actual Fontconfig family for both regular
  and bold choices. Frost resolves an installed monospace family before both
  measuring and painting. A real font-advance test checks regular, bold and
  italic metrics; corrected screenshots were inspected for distinct letters,
  CJK alignment and combining marks.

Frost also limits compatibility-grid shaping to the actual exposed clip,
with a small margin for wide glyphs and italic overhang. This prevents each
split separator or status band from reshaping the whole hidden terminal grid.
The native window rectangles, their backgrounds and the active cursor are
individually clipped in each frontend.

## Reproducible verification

From `runtime/vim9`:

```sh
python3 -m unittest -v test_bridge test_render_windows test_render_delta
python3 benchmark_render.py --lines 100000 --samples 30 --windows 1 2
```

All 49 real Vim tests passed in 48.28 seconds using the combined command
above. The seven new window tests cover split edit/save/GUI syntax;
same-buffer independent viewports and Unicode; Visual sampling invariants;
unsupported peer fallback/recovery; syntax command invalidation; an
asynchronous plugin's explicit invalidation hook; and a real Normal-mode
edit that changes a multiline syntax region in both cached windows. The
scrolling test also checks every reused glyph's absolute row after overlapping
scrolls. The combined run includes all pre-existing input-order/save guards,
search, syntax, property/sign, Unicode and compatibility tests, plus the four
incremental transport tests.

### Observed source-path performance

This measurement uses a 100,000-line, 4,600,000-byte Unicode Rust file,
Vim's runtime Rust syntax, a 30-row by 120-column PTY, and 30 single-line
`Ctrl-E` scrolls. The intentionally unnegotiated legacy transport sends full
frames. Timings include real input, the 16 ms coalescing timer, Vim extraction,
JSON serialization, loopback delivery and Python decoding. They do **not**
measure GUI presentation, FPS, IME latency or an exclusive idle workstation;
other repository builds were running concurrently.

| Windows | Before visible-line cache, median / p95 | After visible-line cache, median / p95 | Full frame bytes, unchanged | Maximum visible runs |
| --- | ---: | ---: | ---: | ---: |
| One | 52.77 / 66.32 ms | 47.27 / 60.65 ms | 94,727–94,825 | 348 |
| Two | 71.49 / 101.14 ms | 89.62 / 115.68 ms | 182,477–182,575 | 672 |

The inactive split remained at `topline=1` while the active split reached
`topline=31`. This is useful evidence that viewports remain independent.
It also identifies extraction/composition work that remains to optimize;
smaller incremental messages alone do not establish 60 FPS scrolling. These
two sequential measurements share the workload, not a controlled exclusive
CPU allocation. The double-window run became slower under concurrent load;
those preliminary data alone do not establish a general end-to-end speedup.

To investigate that concrete regression risk, two temporary runtime copies
were compared with all multi-window, event, compatibility and transport code
held equal. One copy disabled only the new visible-line cache lookup/store.
Both variants processed the same file and 30 scrolls per window count. Round
1 ran cache off then on; round 2 reversed the order. The benchmark additionally
sampled the real Vim process's CPU ticks around the scrolling interval, so
scheduled-out wall time is distinguishable from Vim's CPU work. The copied
runtime's SHA-256 was
`a556b05a4ad3df838aa26c443339c1b4d54d394b67679bd0a895924ebf465659`.

| Round | Windows | Cache off median / p95 | Cache on median / p95 | Vim CPU ms/scroll, off → on |
| --- | ---: | ---: | ---: | ---: |
| 1, off then on | 1 | 53.10 / 71.72 ms | 49.28 / 65.78 ms | 41.667 → 37.667 |
| 1, off then on | 2 | 72.98 / 101.20 ms | 69.95 / 113.93 ms | 62.667 → 60.000 |
| 2, on then off | 1 | 52.87 / 72.95 ms | 53.30 / 65.11 ms | 41.333 → 39.667 |
| 2, on then off | 2 | 84.46 / 128.45 ms | 69.02 / 106.88 ms | 71.333 → 63.667 |

Across these two rounds, average measured Vim CPU per scroll decreased about
6.8% for one window and 7.7% for two. Final frame sizes and glyph counts were
unchanged. Wall-time tails did not improve in every comparison, and the
shared machine remained a source of scheduling noise. The evidence supports
retaining the bounded cache for reduced source CPU work, with no claim of a
universal latency improvement or 60 FPS. Full-frame composition and
serialization remain material work.

`benchmark_render.py --runtime /path/to/bridge.vim` supports repeating such a
controlled runtime comparison. CPU figures are available on Linux; other
platforms still report wall time. Never alter Vim display options to make a
benchmark silently bypass the feature being measured.

### Integration verification record

Frontend compilation, host delta-recovery tests and real-window results are
recorded here when their runs finish. Unavailable IME/HiDPI environments must
remain explicitly unverified rather than being inferred from PTY success.


### Recovery verification (2026-10-20)

Recovered the uncommitted implementation and reran the 49 real Vim Python
regressions: all passed (48.347 s). The focused Rust Vim9 suite passed 24
checks with one opt-in PTY test omitted. `cargo fmt --check` passed.
A broader library run passed 1,613 tests, failed nine and skipped four.
Failures were in process introspection/cleanup, cgroup handling and a Git
status probe, outside the changed Vim9 modules; this environment lacks normal
process/cgroup visibility. The complete library suite is therefore **not**
reported as green. Frontend validation is recorded in each frontend repository.

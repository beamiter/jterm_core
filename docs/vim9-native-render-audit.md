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
| Cursor styling | Vim's `guicursor` supplies block/bar/underline geometry, GUI highlight colors and blink timings. The host clock restarts on input and focus changes, not idle paint revisions. Full source-cell geometry remains available to IME and hit testing. |
| Themes | GUI RGB attributes, links, style changes and named colors are separate from cterm palette entries. Window-local `wincolor` and Normal-only `winhighlight` mappings are supported; general remapping uses compatibility mode. |
| Plugins | Supported signs, text properties, single-line `matchadd()` patterns and byte-based `matchaddpos()` decorations use GUI attributes. Protocol 5 also paints ordinary Insert completion menus. General popups, complex match patterns, conceal and unsupported virtual text use compatibility mode. Async syntax providers use the invalidation hook above. |
| Complex layout/text | Wrapped continuation rows, conceal, diff and RTL/bidi/control text use compatibility mode. Closed folds are native display rows for render protocol 8 hosts ([native closed folds](vim9-native-folds.md)) and compatibility mode for older hosts. Supported help/quickfix windows use native paint with Vim-owned input; terminal and prompt buffers retain compatibility. User options are not silently disabled. |
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

### Plugin match validation (2026-10-21)

The complete eight-module runtime suite passes 85 real Vim PTY checks on
Vim 9.2.0858. Eleven new checks cover plugin priority and search ordering,
case/magic defaults, empty groups, UTF-8 positions, tabs, whole-line and
clipped positions, CursorLine/Visual/property composition, asynchronous
updates, split ownership, built-in matchparen, explicit fallback/recovery,
row deltas and smooth-scroll margins. Screen attribute comparisons use
Vim's actual `screenattr()` as an oracle without decoding opaque attribute
IDs as highlight groups. The Rust Vim9 suite passes 73 tests with its one
opt-in PTY round trip omitted. Frontend GUI validation is recorded separately.

## Rendering follow-up — 2026-10-22

The render producer now reuses each glyph's validated plain-ASCII marker and
constructs only the wire fields on a new paint run. It no longer copies the
entire internal glyph dictionary and removes its cache fields for every run.
In matched Vim profiles, 8,294 `AddRenderRun` calls took 30.21 ms before and
15.15 ms after this change, about 0.58 ms less work per sampled frame.

Two alternating baseline/candidate runs of the protocol-5 scroll benchmark
used a 30×120 viewport, one window, 10,000 source lines and 60 requests each.
Median request-to-ack latency changed from 7.316 to 6.712 ms and from 7.750 to
6.812 ms (approximately 8–12%). The older full-frame input benchmark was noisy
and did not show a consistent latency improvement. These are producer and
transport measurements, not GUI frame rates or physical input-to-photon tests.
The paired raw results are retained in `docs/vim9-render-followup-benchmark.json`.

All four frontends now preserve the full cursor cell when calculating cursor
shape and then clip its paint to the visible viewport. A partially visible row
therefore does not move or resize an underline cursor or the glyph beneath a
block cursor. IME anchors and pointer hit areas continue to use the visible
clipped geometry. Ember keeps foreground color outside the egui layout key,
allowing body text, recoloring and cursor inversion to share shaped galleys.
Anvil and Forge use borrowed text cache lookups and retain Pango width/baseline
measurements, invalidating them with the font context. A million warm lookup
iterations in their isolated production-cache benchmark allocate no strings;
the earlier tuple-key lookup allocated a million. This is a cache benchmark,
not a rendering FPS measurement. Frost retains Iced's existing text cache.

Validation includes the real-Vim bridge and semantic rendering suites, Ember
shape/cursor tests and Frost pixel-geometry/input regressions. GTK production
code and pixel tests compile, and the extracted cache/geometry tests execute;
the GTK pixel test still requires a host with the project's supported GTK
version. Wayland, high-refresh display and OS IME acceptance remain separate.

## Native column rendering — 2026-10-22

Ordinary `cursorcolumn` no longer forces a compatibility frame. The producer
also composes `colorcolumn`, including absolute and textwidth-relative entries,
into native GUI styles. Guides stay window-local and are emitted in semantic
margins as well as the visible viewport. Source text keeps Vim's syntax, match,
search, property and Visual precedence; empty/EOL guide runs have `line=0` and
empty byte ranges and cannot become source-pointer targets.

Tests use real `screenattr()` observations to distinguish Vim's source-text and
EOL branches: Visual suppresses guides on the affected source rows but may
retain guides beyond a short or empty line. The actual `screenpos().curscol`
also distinguishes a wide glyph's first cell from a Normal-mode tab's final
cell. Tab fragments preserve their original byte range; they must not be
coalesced with adjacent ASCII merely because one fragment occupies one cell.
The existing `cursorlineopt=both` line highlight is now recognized explicitly.

Guides outside the visible horizontal range skip guide construction and glyph
segmentation. An ordinary view with guides disabled keeps the original single
paint loop. Wrapped views retain compatibility rendering because the
viewport contract maps one buffer line to one display row. Render protocol 8
added the explicit display-row/source map for closed folds
([native closed folds](vim9-native-folds.md)); wrapped continuation rows still
need one.

## Scratch and window-Normal coverage — 2026-10-22

The producer now permits ordinary nofile, nowrite and acwrite text buffers
through the same native rendering and source-input checks as file buffers.
It preserves Vim's buffer type, modification flags and write/autocommand
semantics. Terminal and prompt buffers retain compatibility; help and quickfix
use the separate native-paint/Vim-input policy described below.

Window-local Normal colors are carried by existing, source-free full-width
background runs before decorations and real text, including blank/EOF rows
and semantic margins. Raw highlight-group caches and global `fg`/`bg` aliases
are not rebound while sampling another split. The line-cache generation
includes the resolved window Normal so palette changes without a buffer edit
cannot reuse stale colors. Consumers use this backdrop for their local clear,
IME/caret fallback and focus colors; old protocols without a backdrop retain
their global Normal behavior. No public Rust API or wire field was added.

## Special text windows: native paint with Vim-owned input

Help and error-list rendering now has a separate admission policy from native
source editing. `RenderableTextBuffer` admits help/quickfix; `NativeTextBuffer`
continues to restrict source-pointer and typed scroll commands to ordinary
file/nofile/nowrite/acwrite text. Metadata explicitly disables both native
input capabilities for the special windows. Frontends use that declaration
only when source metadata is present, preserving old-protocol behavior.
They retire pending scroll/animation state and wait out an old fractional
presentation before passing raw events through the established PTY route.

QuickFixLine is derived from the authoritative quickfix/location-list buffer
identity and current index, not syntax or the text cursor. It is recomposed
without requiring a changedtick. Tests compare native text and highlight
layers with actual Vim, and exercise Enter, Ctrl-] and SGR double-clicks.
Concealment, wrapped continuation rows and unsupported display states continue
to publish explicit compatibility frames; no option is changed automatically.

The full-window-width, empty-text, `line=0` backdrop shape is reserved for
window Normal. QuickFixLine rows retain their actual buffer line with an empty
byte span and full-width spaces, so they cannot change the inferred window
palette and can still paint line decorations through EOL. A guide in a
one-column split uses a space instead of the reserved empty-text signature.
Neither form creates an editable source span.

## Visible whitespace and mouse confirmation — 2026-10-22

Common `listchars` settings now use the semantic paint path: explicit two- or
three-glyph Tab markers, space/lead/trail, both ordinary and narrow NBSP, and
EOL. Glyphs must occupy one Vim display cell. Safe hexadecimal option escapes
are decoded without evaluating an expression. Each Tab cell is emitted
separately with its original byte span, preventing UTF-8 markers from entering
ASCII byte slicing. EOL is display-only. Cache generations include the
window-local list options, so split-specific settings and asynchronous option
changes cannot reuse stale paint. The Tab caret remains one cell; a wide source
character retains its full width.

Default `^I` Tab geometry, unsupported marker fields, active horizontal-edge
markers and EOL-induced wrapped continuation rows still use explicit
compatibility. Seven new real-Vim checks compare glyphs and highlight ordering
with `screenstring()`/`screenattr()` oracles, exercise source-pointer and save
semantics, and cover split-local updates, horizontal clipping and overscan.

Mapped GTK dialogs now own keyboard input before workspace capture handlers.
Body clicks in all four frontends restart the original native blink wait,
including a repeated click at the same source cell. Frost routes an inactive
embedded body's first click after establishing pane focus, retains current
modifiers and click counts, and includes Ctrl/Alt bits in app mouse reports.

The complete real-Vim runtime discovery run passes 154 tests with one optional
completion-metadata test skipped (155 total, 149.217 s). The shared Rust
viewport test also verifies that Unicode replacement markers keep their
original space/Tab bytes and do not move the EOL source target.

A single ordinary-`nolist` before/after scroll comparison measured 18.0 ms of
Vim CPU per scroll in both versions. Median input-to-semantic-frame time was
34.710/35.421 ms; frame sizes and run counts were identical. Concurrent builds
and the single sample pair limit this check: it detects no material CPU
regression here and establishes no GUI frame-rate or latency improvement.
Raw results and runtime hashes are in
`docs/vim9-whitespace-followup-benchmark.json`.

The rebuilt Forge executable was also inspected in an isolated X11 window
with minimal Vim startup settings. `notermguicolors` and deliberately different
cterm colors leave the buffer at native GUI Normal `#1e1e2e`, with exact
SpecialKey `#f38ba8` and NonText `#89b4fa` pixels. The captured source includes
Tab, trailing spaces, CJK and a combining mark; Vim's observation file confirms
the original text and the requested list options. The inspected screenshot is
`forge/target/vim-font-fix/native-whitespace.png` in the sibling checkout.
It is a local verification artifact and is not included in the source release.

Frontend follow-up validation: Forge passes five focused checks, including
actual X11 dialog keyboard ownership, repeated body-click focus/blink,
single-cell Unicode markers and the real font keyboard/wheel regression.
Anvil's final all-targets typecheck passes. Frost passes 37 buffer, 27 font and
19 body-filter checks, plus its separately executed real dual-Vim mouse test;
these filters overlap and are not summed. Ember passes 34 buffer checks.
Frost's event snapshot retains entry identity, presentation and event-time
modifiers; source replacement or stationary viewport changes cannot reinterpret
an old click as raw coordinates in another buffer. Its modifier ledger is
updated before the child widget decides click ownership. The real-Vim test
checks Ctrl/Alt press/drag/release isolation and exactly one paired release
when a host modal interrupts an existing app gesture.

# Native GUI rendering for Vim buffers

## Product requirement

The editor text is the primary surface of the Vim9 IDE. Native project trees,
search controls and toolbars are useful, but the buffer itself must benefit
from the host's modern font and graphics stack.

Vim remains the editing authority: buffer contents, undo, mappings, operators,
registers, plugins and save behavior belong to the real Vim process. The host
owns the visible buffer's text drawing, RGB colors, decorations and cursor.
The protocol transports drawing data derived from Vim's buffer and GUI
highlight attributes, independently of terminal escape sequences.

## Buffer-first defaults

The four frontends share `Vim9UiConfig` and the same TOML section:

```toml
[vim9]
native_buffer = true
toolbar = false
buffer_tabs = false
status_bar = false
project_panel = false
tool_panel = false
```

Missing fields inherit these defaults, including when an older configuration
has no `[vim9]` section. Enabling a project panel or toolbar does not toggle the
buffer renderer. Disabling the native buffer uses the working Vim display
without enabling workbench controls. `tool_panel` covers the optional search,
build, diagnostics and output UI. Window decorations, explicit editor entry,
font controls, Vim's own command line and error feedback remain accessible.

Hidden controls reclaim their layout space. Project scanning is deferred until
the project UI is requested. Workbench shortcuts must not put focus into an
invisible field; save and buffer font zoom remain editor actions. A frontend
may reveal a tool in response to an explicit command. Normal `vim` in a shell
still follows the explicit-entry contract and does not switch app modes.

## Rendering boundary

| Layer | Responsibility |
| --- | --- |
| Vim 9 | Editing state, syntax groups, display positions and GUI highlight attributes |
| `jterm_core::vim9` | Authenticated channel, render subscription, bounded frames and snapshot ownership |
| Anvil / Forge | GTK and Pango buffer text and native decoration drawing |
| Ember | egui buffer text and GPU decoration drawing |
| Frost | Iced buffer text and GPU decoration drawing |
| PTY adapter | Real Vim keyboard/mouse input and the compatibility display outside a supported GUI buffer |

This first protocol retains Vim's logical row/column geometry so keyboard
motions, mouse positions, wide characters and tabs address the same text in
Vim and the host. The host converts those positions to pixels using its
editor metrics. This does not yet provide arbitrary proportional code layout,
variable-height inline widgets or a complete external UI for every Vim window.

## Data contract

The optional `set_render` subscription adds a separate render event. Ordinary
IDE metadata continues to work without a render consumer. A
`Vim9RenderFrame` identifies the current buffer, window, revision and changed
tick, describes the viewport and cursor, and supplies styled text runs.

A run contains its UTF-8 text, byte range in the original buffer line, logical
position and display width. The source bytes and the logical display width
are different units; consumers must not equate either with a Unicode scalar
count. Vim supplies the display geometry. Runs with matching styles can be
drawn together through the host's text stack.

Coalesced ASCII runs keep a one-byte/one-cell source mapping. Expanded tabs
remain separate when their displayed cell width differs from their source byte
length. Non-ASCII graphemes keep their own Vim-owned display advance. This is
also the mapping used by native overlay attachment points; expanding a tab
must never shift the source offset of the text that follows it.

Styles carry GUI foreground, background and special colors plus text
attributes. GUI colors come from Vim's resolved highlight groups, including
`guifg`, `guibg` and `guisp`. A missing GUI color inherits the documented
default style. The terminal's indexed palette is not the style source.

Hosts must validate a frame against the current buffer/window and viewport
before displaying it. An unsupported, stale or missing frame reveals the
working compatibility display. A compatibility reason is an observable state,
not an indication that the buffer has stopped accepting Vim input.

## Correctness before additional visual features

The following checks establish the rendering boundary:

1. Disable `termguicolors`, give a syntax group different `ctermfg` and `guifg`
   colors, and verify that the visible GUI buffer uses `guifg`.
2. Exercise insert, normal and Visual modes against the real Vim process;
   verify that save and undo operate on the edited buffer.
3. Check tabs, Chinese/wide characters, combining characters and byte offsets.
   Mouse selection and the cursor must refer to the same positions as Vim.
4. Change a highlight group without changing the buffer text. The GUI style
   must update even when the buffer's changed tick stays constant.
5. Resize, scroll and switch buffers. A frame from the previous geometry or
   buffer must not remain painted over the current editor.
6. Open an unsupported view and return to a supported buffer. Compatibility
   must remain usable and the GUI surface must resume automatically.
7. Keep rendering work bounded to the visible area and avoid regenerating an
   unchanged frame while idle. Metadata tests alone do not establish input
   latency or display performance.

Host window tests complement the shared protocol's real Vim/PTY tests.
Protocol assertions cannot prove the actual pixels, font fallback, input
method placement or compositor behavior on the user's desktop.

## Pixel geometry and native overlay attachment points

`Vim9RenderMetrics` is the common conversion from Vim cells to host logical
pixels. The frontend supplies its actual editor origin, cell width and line
height; `rect`, `cell_rect`, `cursor_rect` and `cell_at` reuse those metrics for
paint, input-method placement and future overlays. Fractional advances remain
fractional. Rounding individual cells creates drift at non-integer zoom/DPI.
Invalid or non-finite metrics do not produce paint or input geometry.

The core provides a source-aware attachment API without depending on GTK,
egui or Iced:

| API | Purpose |
| --- | --- |
| `frame.anchor_at_cell(row, column)` | Resolve a visible glyph to its window, buffer, changed tick, UTF-8 byte range and cell rectangle |
| `frame.cursor_anchor()` | Attach to the current glyph, empty line or insertion endpoint |
| `anchor.position()` | Capture a source position for re-resolution after scroll or zoom |
| `frame.anchor_for_position(&position)` | Resolve that exact window/buffer generation in the current viewport |
| `frame.anchor_is_current(&anchor)` | Reject an attachment from a different paint revision or source location |
| `metrics.rect(&anchor.rect)` | Place the host widget using the same transform as the painted text |

Anchors exclude gutters, line backgrounds, end-of-buffer decorations, split
separators and terminal-owned status/command rows. A wide glyph, combining
cluster or expanded tab returns its full source range. Plain ASCII can resolve
to a single byte/cell inside a shaped run. Two splits displaying the same
buffer remain distinct through the Vim window identity.

An asynchronous hover result belongs to its owning editor session. Drop it on
session exit/re-entry, even if another Vim instance later reuses the same
window IDs. Before painting, check the current frame. If the revision changed,
re-resolve the captured source position; hide the result when it is off-screen,
the window closed, the buffer changed, the changed tick differs or the bridge
entered compatibility mode. Never reuse an old pixel rectangle after scroll,
resize, font changes or DPI changes. The metadata is an attachment contract,
not an edit command or permission to mutate a buffer.

## Rendering work

GTK frontends retain the current shaped text/background scene across
cursor-only, focus and composition updates. Changed text, GUI styles, view
rectangles, font metrics, font settings or scale invalidate that scene. The
cursor and preedit layer remain dynamic. Frost groups runs by window before
painting so all panes do not repeatedly scan every run. Ember uses the same
fractional geometry for its visible caret and operating-system IME anchor.

These are bounded renderer changes, not a claim of measured end-to-end latency
on every GPU/compositor. Native smoke tests and real Vim protocol tests cover
different parts of the boundary.

## Further work enabled by this boundary

The next UI extensions can consume explicit Vim state and draw native
completion menus, command/search bars, diagnostic decorations, hover cards and
inline information. Each needs an input and state contract as well as a
renderer. Changes to fonts, spacing or animation must preserve the mapping
between the displayed text and Vim's editing positions.

Hover boards, Markdown previews, decoded images and color pickers can share the
attachment API while keeping their own native content renderers and input
handling. This change supplies the attachment and lifetime contract; it does
not yet implement those four complete features. Arbitrary proportional editing,
variable-height inline blocks and supported native rendering of all Vim popup,
fold and wrapped-line states still need additional protocol work.

See [the IDE guide](vim9-ide.md) and
[the runtime protocol](../runtime/vim9/README.md) for the implemented
capabilities and compatibility limits.

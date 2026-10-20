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

## Further work enabled by this boundary

The next UI extensions can consume explicit Vim state and draw native
completion menus, command/search bars, diagnostic decorations, hover cards and
inline information. Each needs an input and state contract as well as a
renderer. Changes to fonts, spacing or animation must preserve the mapping
between the displayed text and Vim's editing positions.

See [the IDE guide](vim9-ide.md) and
[the runtime protocol](../runtime/vim9/README.md) for the implemented
capabilities and compatibility limits.

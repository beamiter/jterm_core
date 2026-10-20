# Vim9 native editor runtime

`bridge.vim` runs inside an actual Vim 9 editor. The frontend sends normal
keyboard input to the PTY. Its separate authenticated native Vim channel
provides metadata, explicit IDE actions and opt-in GUI buffer paint frames.
Supported buffer text is rendered from those frames with GUI colors; PTY
output remains the live compatibility surface for unsupported Vim displays.
Vim continues to own editing, undo, registers, mappings, macros, windows,
completion and plugins; there is no second copy of the document in this bridge.

## Requirements and loading

Use Vim 9.0 or later with `+vim9script`, `+channel`, `+job`, `+timers` and
`+quickfix`. The real-editor integration suite has been run on Vim 9.1.0016
with distribution backports. The runtime avoids newer Vim9 classes and newer
channel-server APIs. It is sourced after ordinary Vim startup, using `-S`, so
the user's vimrc and plugins are loaded normally. It does not install a vimrc,
change the user's mappings, disable swap recovery, or force the `hidden` option.

The host starts its loopback listener before launching Vim and supplies these
environment values only to the new editor process:

| Name | Meaning |
| --- | --- |
| `JTERM_VIM9_ADDR` | The host's ephemeral `127.0.0.1:port` address |
| `JTERM_VIM9_TOKEN` | A random secret for this single editor session |
| `JTERM_VIM9_ROOT` | Absolute project root for builds and project search |

The runtime immediately removes `JTERM_VIM9_TOKEN` from Vim's environment, so
build tools started by the runtime do not inherit that credential. The token
is retained only in a script-local variable until authentication succeeds;
that copy is then cleared. Address and project-root environment values remain.

If the environment is absent, sourcing the script is a no-op. Loading it twice
does not register duplicate callbacks. A lost bridge leaves the editor alive.

## Wire protocol

Protocol version 1 is UTF-8 JSON with one object per newline. The native Vim
channel uses `mode: 'nl'`; `json_decode()` and `json_encode()` handle object
encoding. It deliberately does not use Vim's `mode: 'json'`, whose built-in
array protocol also recognizes executable `ex`, `expr`, `normal` and `call`
messages outside a user callback.

Vim sends the first message:

```json
{"event":"hello","protocol":1,"token":"session secret","version":901,"vim9":true,"pid":12345,"seq":1}
```

After validating that hello, the host sends:

```json
{"action":"auth","protocol":1,"token":"session secret"}
```

The runtime validates both token type/value and protocol type/value. It rejects
editing requests until that exchange completes and closes the channel on an
incorrect authentication message. Authentication success publishes the first
state; it does not need a separate response with an artificial request id.

Subsequent requests have a positive integer `id` and one action. For example:

```json
{"id":1,"action":"open","path":"/project/src/main.rs","line":20,"column":5}
{"id":2,"action":"save"}
```

The runtime replies with `event: "response"`, the matching `id`, Boolean `ok`,
and an `error` string when applicable. Exceptions are reported through this
response rather than suppressed or inserted into the user's command line.
Async search failures use `{"event":"error","error":"..."}`. Successful
build/search responses acknowledge job startup; their completion is published
in subsequent state. Events include an increasing `seq` and `protocol: 1`.

### Actions

| Action | Arguments | Behavior |
| --- | --- | --- |
| `open` | `path`; optional `line`, `column` | Open a real Vim buffer; explicit positions use one-based line and byte column |
| `buffer` | Positive integer `buffer` | Select an existing buffer id |
| `goto` | Positive integer `line`; optional `column` | Move in the current buffer, including unnamed/modified buffers; one-based byte column |
| `next_buffer` / `previous_buffer` | None | Cycle listed buffers with native modified-buffer checks and remembered views |
| `navigate_back` / `navigate_forward` | None | Traverse the current Vim window's real jumplist, sharing history with `Ctrl+O` / `Ctrl+I` |
| `close_buffer` | Positive integer `buffer` | Unlist a saved buffer with `:bdelete`; always refuse modified buffers, including hidden ones |
| `close_current_buffer` | None | Resolve the current buffer inside Vim after earlier commands, then apply the same protected close |
| `split` | Boolean `vertical` | Native `:vsplit` or `:split` of the current buffer |
| `save` | None | Native `:update`, including the user's write hooks |
| `save_all` | None | Native `:wall`, including ordinary Vim write errors |
| `build` | Optional/empty `command` | Start an async shell job; an empty command uses expanded `makeprg` |
| `save_and_build` | Optional/empty `command` | Validate the command, `:wall`, and start a build only after successful writes and no remaining modified buffers |
| `cancel_build` | None | Request termination of the running build job |
| `search` | Nonempty, single-line `query` | Async literal project search using `rg`, with Smart case by default |
| `search_with_options` | `query`; `options` object | Async search with `case` (`smart`, `sensitive`, `ignore`), Boolean `regex`/`whole_word`, and one optional string `glob` |
| `cancel_search` | None | Cancel the current search; retain the preceding quickfix list and invalidate late callbacks |
| `definition` | None | `:LspGotoDefinition`, or native tags/tagfunc |
| `hover` | None | `:LspHover` |
| `references` | None | `:LspShowReferences` |
| `rename` | Nonempty `name` | `:LspRename name`; whitespace/command separators are rejected |
| `format` | None | `:LspFormat` |
| `next_diagnostic` | None | Native `:cnext` |
| `previous_diagnostic` | None | Native `:cprevious` |
| `set_render` | Boolean `enabled` | Subscribe/unsubscribe to native GUI buffer paint frames after the session is connected |
| `snapshot` | None | Force a current metadata state and reconcile the subscribed native paint frame |
| `quit` | None | Native `:qall` without `!`; acknowledge from `VimLeavePre` once Vim accepts exit |

Requests are limited to 64 KiB. Unknown actions, invalid types, invalid buffer
ids and nonpositive positions receive an error. Filenames are passed through
`fnameescape()`; filenames containing newlines are not supported. A file that
already has a buffer is matched by its exact absolute name, avoiding Vim's
pattern-based buffer lookup fallback.

### Native GUI buffer frames

The protocol is backwards compatible: no `render` events are produced before
an authenticated host sends `{"id":3,"action":"set_render","enabled":true}`.
In Rust, wait for `snapshot().connected`, send `Vim9Command::SetRender`, then use
`render_snapshot()` to cheaply read `Option<Arc<Vim9RenderFrame>>`. The same Arc
is available as `snapshot().render`. Turning the subscription off publishes
`render_disabled`; disconnect clears the retained frame as well.

Each `render` event has the following fields, in addition to `event`, `seq`
and `protocol`:

| Field | Meaning |
| --- | --- |
| `revision` | Increasing native paint revision; older revisions cannot replace newer retained frames |
| `supported`, `fallback_reason` | Whether the host can draw this state; otherwise show the actual Vim compatibility view immediately |
| `buffer`, `window`, `changedtick`, `mode` | Vim-owned identities and editing state; metadata transitions cannot keep another buffer's native frame |
| `rows`, `columns` | PTY screen dimensions used to produce this paint frame |
| `view` | `{row,column,rows,columns}` for the exact single-window opaque paint rectangle, including number gutters; command/status rows are outside |
| `cursor` | `{row,column,width,visible}`, using Vim's real screen position and wide-character width |
| `default_style` | GUI Normal style, with explicit light/dark RGB fallback only when Normal has no GUI color |
| `runs` | Bounded native glyph runs with `row`, `column`, `text`, `cells`, `style`, `line`, `byte_start`, `byte_end` |

Screen row/column positions are **zero-based**. A run's `line` is a one-based
buffer line, or zero for gutter/end-of-buffer decoration. Byte ranges are
zero-based, half-open UTF-8 offsets into the original buffer line. Tab text is
expanded to spaces while its byte range continues to refer to the original
tab. Non-ASCII graphemes, including a base character and combining marks, stay
in individual runs; adjacent ASCII with the same style may be combined for
GUI shaping. Hosts must respect `cells` and cannot independently reflow text
while continuing to use Vim's input coordinates.

Styles contain nullable packed `0xRRGGBB` `foreground`, `background` and
`special`, plus Boolean `bold`, `italic`, `underline`, `undercurl`,
`strikethrough` and `reverse`. Highlight links are resolved in Vim and values
come from `synIDattr(..., 'gui')`. Vim's own `v:colornames` table resolves named
GUI colors. `ctermfg`, `ctermbg`, terminal escape sequences and terminal
palettes are not consulted. Visual selection and supported Search/text-property
highlights are already composed into the run style.

Native paint is driven by a separate 16 ms input/view coalescer. Buffer glyphs
and syntax styles are reused when buffer contents and view inputs are
unchanged; direct `:highlight` changes, property changes and linked groups are
reconciled too. Neither the 40 ms metadata coalescer nor 750 ms idle poll is the
primary typing paint path. A supported frame is bounded to 200 rows, 400
columns, 4,096 characters/16 KiB per source line, 2,048 visible text properties
and 900,000 JSON bytes. Unsupported or oversized frames contain a reason and
no misleading partial native document. The receiver enforces its 1 MiB frame
limit and validates geometry, RGB values and run text before retaining an Arc.

See [the native buffer display guide](../../docs/vim9-ide.md#native-buffer-display)
for the precise supported feature set and compatibility cases. The protocol
does not claim a full external-UI replacement for every Vim plugin/display
feature, and the bridge never changes wrap, conceal or other editing options
to conceal an unsupported case.

GUI file/buffer navigation remembers and restores each buffer's native
`winsaveview()` cursor and scroll position. New-file `BufRead`/`BufEnter` hooks
retain their own positioning unless coordinates were explicitly requested.
Ordinary Vim commands continue to follow the user's settings, including
`startofline`. Native `goto` and explicitly positioned `open` in the current
file add their origin with Vim's `m'` command; file changes already record a
jump through Vim's buffer commands. Back/forward use unmapped normal commands
on that same window's jumplist. An explicit count before Ctrl-I prevents Ex
from trimming its Tab byte as whitespace. No action forces away modified data. Actions that change editor
state refuse a pending operator, command-line prompt, mapping or completion;
they never synthesize Escape to clear what the user is entering. A narrow
exception handles `save`, `save_all` and `save_and_build` arriving during
transient Escape/keycode/mapping decoding (`state()` flags `m` or `w`): the
runtime queues one save until `SafeState` or `SafeStateAgain`, with a two-second
deadline and the originating current buffer recorded. Pending operators,
counts/finds, command lines and completion are rejected immediately. Other
native actions are rejected while a save waits, except state requests and
build/search cancellation. A changed current buffer, timeout or disconnect clears
the queued save; it is never replayed later. Build validation occurs before
queuing a save-and-build and again before its writes execute.

### Published state

`event: "state"` includes `mode`, current absolute `file`, project `root`,
current Vim `cwd`, `line`, byte `column`, Boolean `modified`, `changedtick`,
`filetype`, effective `encoding`, `line_count`, current-tab `window_count`,
Boolean `waiting_for_input` for a deferred save,
`capabilities`, `buffers`, `diagnostics`, `build` and
`search`, `navigation` and `quickfix`. Additional fields are forward-compatible.

`navigation` contains Boolean `can_go_back` and `can_go_forward` from the
current window's bounded native jumplist. `quickfix` contains `source`
(`search`, `build` or `quickfix`), `title`, one-based `index` and `count` of the
actual current quickfix list. This metadata does not label every item in the
combined diagnostics, which can additionally include location-list and LSP
results. A user-created quickfix list has source `quickfix`.

Buffers contain `id`, absolute `name`, Boolean `modified` and Boolean `current`.
The runtime constructs these fields explicitly; it never serializes complete
buffer/window dictionaries or their plugin variables, which may contain
Funcrefs, channels, cyclic objects or private plugin state.

Diagnostics contain `filename`, `line`, byte `column`, `text` and `kind`
(`error`, `warning`, `info`, `hint`). Sources include Vim's quickfix list, the
current window's location list, and current-buffer diagnostics exposed by the
supported Vim9 LSP plugin. Duplicate entries are removed. LSP's default UTF-16
columns are converted to Vim byte positions. For a visual-column quickfix item
in a displayed buffer, Vim's `virtcol2col()` performs the conversion. If that
buffer has no rendering window, navigation falls back to the correct line's
first byte rather than guessing a byte position from a visual column.

At most 256 buffers and 256 diagnostics are published. Boolean
`buffers_truncated` and `diagnostics_truncated` indicate omitted data;
`buffers_omitted` and `diagnostics_omitted` supply counts. Payloads are trimmed
below 900,000 bytes to leave room within the host's 1 MiB frame limit. Snapshots
are coalesced at 40 ms, with a 750 ms reconciliation for changes such as direct
plugin calls to `setqflist()`. Diagnostic work is cached by quickfix/location
list id and changedtick, buffer identity/change tick, and LSP update events.

## Builds and search

Builds use Vim's `job_start()` with its configured shell and shell command flag.
The command is either the explicit user-entered build command or `makeprg`;
this is the sole action that deliberately accepts shell syntax. It runs from
the project root and does not implicitly save buffers. A second build is
rejected while one is running. Stdout and stderr are collected asynchronously;
the job is finished only after both exit and channel-close callbacks arrive,
so the final output is included in quickfix parsing. The explicit
`save_and_build` action first validates the command and running-job state,
then calls `:wall`. A readonly/unnamed buffer, a failing write hook or remaining
modified buffers prevent the build. Some earlier buffers may have been saved
before `:wall` reports a later error, following ordinary Vim behavior.

Quickfix input is bounded to 10,000 lines of 2,048 characters each;
`build.output` independently keeps the latest 80 output lines, including after
that capture fills. `build.truncated` counts all output lines omitted from the
native tail. State also includes `build.running`, `command`, nullable `exit_code` and
`cancelled`. Parsing uses Vim's `errorformat` captured at build start. An
errorformat directory-stack entry anchors relative filenames to the build's
root even if the user changes Vim's directory while the job is running.
The injected directory seed is removed from the parsed items before publishing
the list; it is not a build problem and must not inflate the displayed count.

Search uses an argv list and a `--` argument boundary. The compatible default
is `--fixed-strings --smart-case`. Options are strictly validated before an
existing search is replaced: `regex` omits `--fixed-strings`, `whole_word` adds
`--word-regexp`, `case` selects an explicit case flag, and `glob` supplies one
`--glob` argument. Queries and globs are bounded to 4096 UTF-8 bytes and one
line. No shell interpretation or comma/space splitting occurs. Searches use
`--no-config` so `RIPGREP_CONFIG_PATH` cannot change the selected behavior or
JSON framing. They still honor project ignore files, include hidden files,
and exclude `.git` after any user glob.

Results are limited to 256 matching lines. Their omitted count is stored in
that quickfix list's context, so a newer failed or cancelled job cannot alter
the retained list's truncation indicator. Search state includes the actual
`options` and Boolean `cancelled`, alongside the existing query/count/errors.
Cancellation publishes exit code 130, stops the running job and invalidates
its generation before late output or exit callbacks can publish a list. It
keeps the previous quickfix list; partially collected rows are not published.
A new search resets cancelled state and replaces a running job. Successful
search, including no matches, replaces the current quickfix list. An rg error
(including invalid regex syntax) publishes a visible error and preserves the
previous list. A later completed build or search can replace that list; full
underlying Vim buffer data is unaffected.

## Optional Vim9 language features

The supported adapter is [Yegappan's Vim9 LSP](https://github.com/yegappan/lsp).
Its public commands are listed in the action table. The user installs and
configures that plugin and its language servers in their normal Vim setup;
this bridge does not download them. Their registered commands determine the
advertised `definition`, `hover`, `references`, `rename` and `format`
capabilities. A native Vim tags file or configured `tagfunc` can also supply
`definition` without this plugin. A registered command is not proof that a
particular language server supports every action; its normal Vim errors and
messages still apply.

The adapter observes `User LspDiagsUpdated`, `LspAttached` and `LspDetached` and
uses `lsp#diag#GetDiagsForBuf()` when available. Other Vim plugins still load
and work normally in the editor; native buttons for other command vocabularies
need an explicit adapter rather than guessed commands.

## Real-editor tests

From the core repository:

```sh
VIM9_TEST_VIM=/absolute/path/to/vim python runtime/vim9/test_bridge.py
```

`VIM9_TEST_VIM` is optional when `vim` is on PATH. These Python-standard-library
tests create isolated files, actual PTYs and private loopback sockets, then
drive native Vim and the shipped script. They verify authentication and bad
requests, actual edits/undo/save, user vimrc mappings, escaped paths, remembered
views, dirty-buffer/exit protection, preservation of pending input, immediate
Escape/save sequencing, deferred-save timeout without replay, consecutive
navigation/current-close requests, Save All,
async build/cancel/output, working-directory correctness, secret cleanup,
literal rg search, result caps and asynchronous errors. Provider-routing tests
use user-defined native Vim commands; they do not claim to test an external
language server. The suite is skipped if no Vim executable is available.

These tests do not validate frontend font rendering, desktop IME integration,
mouse input or all user plugins; those are frontend/desktop verification tasks.

## Primary references

- [Official Vim channel/job documentation](https://github.com/vim/vim/blob/master/runtime/doc/channel.txt): NL framing, callbacks, async jobs and channel lifetime.
- [Official Vim9 documentation](https://github.com/vim/vim/blob/master/runtime/doc/vim9.txt): compiled `def` functions and coexistence with legacy user scripts.
- [Official Vim builtin documentation](https://github.com/vim/vim/blob/master/runtime/doc/builtin.txt): buffer snapshots, quickfix, state checks, filename escaping and view preservation.
- [Official Vim autocommand documentation](https://github.com/vim/vim/blob/master/runtime/doc/autocmd.txt): mode/text/quickfix events and pending-state behavior.
- [Official Vim startup documentation](https://github.com/vim/vim/blob/master/runtime/doc/starting.txt): normal vimrc/plugin loading and the `-S` startup hook.
- [Vim9 LSP public documentation](https://github.com/yegappan/lsp/blob/main/doc/lsp.txt): supported semantic commands, diagnostics and user events.

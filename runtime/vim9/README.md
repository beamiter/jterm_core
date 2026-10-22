# Vim9 native editor runtime

Host presentation defaults and the source-aware native overlay attachment API
are documented in [Native GUI buffer](../../docs/native-gui-buffer.md). The
render cursor optionally includes `line` (one-based) and `byte_column`
(zero-based UTF-8) so empty lines and insertion endpoints have real source
anchors. Older frames without these fields still support glyph anchors. ASCII
run coalescing preserves one-byte/one-cell mapping; expanded tabs stay separate
when that mapping differs.

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

After authentication, `g:jterm_vim9_connected` becomes `v:true`,
`g:jterm_vim9_render_protocol` records the negotiated paint version, and
`User JTermVim9Ready` fires. Disconnect or exit sets them to `v:false` and `0`
and fires `User JTermVim9Disconnected` once. These events let a user vimrc
distinguish a live native session from inherited startup environment values.
The older `g:jterm_vim9_bridge_loaded` marker remains a load/idempotency marker.
An exception in a user lifecycle autocommand does not break the channel.

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
{"action":"auth","protocol":1,"token":"session secret","render_protocol":2}
```

The runtime validates both token type/value and protocol type/value. It rejects
editing requests until that exchange completes and closes the channel on an
incorrect authentication message. Authentication success publishes the first
state; it does not need a separate response with an artificial request id.
The optional `render_protocol` negotiates native paint transport separately
from the command protocol. Version 1, also the default when omitted, receives
complete frames. Version 2 additionally accepts row deltas. Version 3 adds
source-bound scroll/pointer input and two real margin rows per viewport edge.
Version 4 retains those semantics and extends each margin to eight rows for
continuous wheel transitions and short bursts. Version 5 adds the optional
`completion` overlay for ordinary, fully visible Insert completion menus. Older
hosts continue using the live compatibility menu. Existing command and metadata
messages continue to use `protocol: 1`.

Protocol-3 and newer window metadata also has an optional `native_scroll`
capability, independent of `native_pointer`. It permits source-bound scrolling
in idle Normal and Insert mode, without requiring mouse input, standard click
mappings or a particular `virtualedit` setting. Producers without this field
retain compatibility scrolling. Completion and busy modes do not admit it.

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
| `scroll` | Positive `window`; signed nonzero `lines` with magnitude ≤120 | Scroll that window in the current tab using Vim's own viewport semantics; preserve focus and buffer contents; positive counts move down |
| `save` | None | Native `:update`, including the user's write hooks |
| `save_all` | None | Native `:wall`, including ordinary Vim write errors |
| `build` | Optional/empty `command` | Start an async shell job; an empty command uses expanded `makeprg` |
| `save_and_build` | Optional/empty `command` | Validate the command, `:wall`, and start a build only after successful writes and no remaining modified buffers |
| `cancel_build` | None | Request termination of the running build job |
| `search` | Nonempty, single-line `query` | Async literal project search using `rg`, with Smart case by default |
| `search_with_options` | `query`; `options` object | Async search with `case` (`smart`, `sensitive`, `ignore`), Boolean `regex`/`whole_word`, and one optional string `glob` |
| `cancel_search` | None | Cancel the current search; retain the preceding quickfix list and invalidate late callbacks |
| `definition` | None | Advertised workbench provider, then `:LspGotoDefinition` or native tags/tagfunc |
| `hover` | None | Advertised workbench provider, then `:LspHover` |
| `references` | None | Advertised workbench provider, then `:LspShowReferences` |
| `rename` | Nonempty `name` | `:LspRename name`; whitespace/command separators are rejected |
| `format` | None | Advertised workbench provider, then `:LspFormat` |
| `workbench` | Fixed `command`; current `window`, `buffer`, `changedtick` | Invoke an available Simple action in the user's vimrc; reject a changed source or removed capability |
| `next_diagnostic` | None | Native `:cnext` |
| `previous_diagnostic` | None | Native `:cprevious` |
| `set_render` | Boolean `enabled` | Subscribe/unsubscribe to native GUI buffer paint frames after the session is connected |
| `render_resync` | None | Request a fresh complete paint frame without changing Vim state; the Rust receiver reserves request id zero for this recovery action |
| `snapshot` | None | Force a current metadata state and reconcile the subscribed native paint frame |
| `quit` | None | Native `:qall` without `!`; acknowledge from `VimLeavePre` once Vim accepts exit |

Requests are limited to 64 KiB. Unknown actions, invalid types, invalid buffer
ids and nonpositive positions receive an error. Filenames are passed through
`fnameescape()`; filenames containing newlines are not supported. A file that
already has a buffer is matched by its exact absolute name, avoiding Vim's
pattern-based buffer lookup fallback.

The frontend Quick Open query syntax (`path:line[:byte-column]` and
`:line[:byte-column]`) resolves to the existing typed `open` and `goto`
actions. `python3 runtime/vim9/test_quick_navigation.py` checks real Vim
navigation with literal colon/Ex-special filenames, UTF-8 byte positions,
jumplist round trips and modified or unnamed current buffers.

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
| `view` | `{row,column,rows,columns}` for the active window's exact opaque paint rectangle, including number gutters; command/status rows and split separators are outside |
| `cursor` | `{row,column,width,visible}`, using Vim's real screen position and wide-character width; optional `appearance` carries the resolved `guicursor` shape, percentage, GUI style and millisecond `blinkwait`/`blinkon`/`blinkoff` |
| `windows` | Every supported ordinary window in the current tab: `window`, `buffer`, `changedtick`, `view`, `cursor`, `topline`, `botline`, `leftcol`, and `textoff`; only the active cursor is visible |
| `default_style` | GUI Normal style, with explicit light/dark RGB fallback only when Normal has no GUI color |
| `runs` | Bounded native glyph runs with owning `window`, `row`, `column`, `text`, `cells`, `style`, `line`, `byte_start`, `byte_end` |

Screen row/column positions are **zero-based**. A run's `line` is a one-based
buffer line, or zero for gutter/end-of-buffer decoration. Byte ranges are
zero-based, half-open UTF-8 offsets into the original buffer line. Tab text is
expanded to spaces while its byte range continues to refer to the original
tab. Non-ASCII graphemes, including a base character and combining marks, stay
in individual runs; adjacent ASCII with the same style may be combined for
GUI shaping. Hosts must respect `cells` and cannot independently reflow text
while continuing to use Vim's input coordinates.

All run coordinates remain global screen coordinates, including in vertical
and mixed splits. Hosts paint the union of window rectangles, leaving the
live PTY surface visible in separators, status lines and command rows. Every
run must fit its owning window. Legacy single-view frames without `windows`
or run ownership remain readable by the Rust model.

For render transport version 2, a `render_delta` event contains the same
top-level frame metadata, plus `base_revision` and `row_updates`, and omits
the full `runs` array. Each row update is `{row, runs}` and replaces **all**
runs on that global screen row, including neighboring vertical splits.
An empty `runs` clears the row; empty `row_updates` can update only the cursor
or other metadata. The base must exactly match the receiver's retained
revision. Layout, window identity or default-style changes require a complete
frame. The runtime chooses a delta only when its encoded message is smaller,
and emits a full checkpoint at least every 120 published revisions.

The Rust receiver validates and applies a delta atomically before publishing
an immutable `Arc<Vim9RenderFrame>`. On a missing or invalid base it hides
native paint, requests `render_resync`, and continues processing ordinary
metadata, keyboard input and editing commands. Old revisions cannot restore
stale paint during recovery. Metadata that temporarily hides a frame does
not discard the independent transport baseline. Subscription disable and
disconnect clear that baseline; re-enable starts with a complete frame.

Styles contain nullable packed `0xRRGGBB` `foreground`, `background` and
`special`, plus Boolean `bold`, `italic`, `underline`, `undercurl`,
`strikethrough` and `reverse`. Highlight links are resolved in Vim and values
come from `synIDattr(..., 'gui')`. Vim's own `v:colornames` table resolves named
GUI colors. `ctermfg`, `ctermbg`, terminal escape sequences and terminal
palettes are not consulted. Visual selection and supported Search/text-property
highlights are already composed into the run style.

Native paint is driven by a separate 16 ms input/view coalescer. Buffer glyphs
and syntax styles are cached per window and visible source line. Overlapping
scrolls reuse unchanged lines; a changed buffer tick invalidates all cached
syntax for that buffer, including multiline syntax dependencies. Direct
`:highlight` changes, property changes and linked groups are reconciled too.
Syntax/FileType and command-mode transitions invalidate syntax caches; an
asynchronous syntax provider that changes rules without changing text must
emit `doautocmd <nomodeline> User JTermVim9RenderInvalidate`. Neither the 40 ms
metadata coalescer nor 750 ms idle poll is the primary typing paint path.
A supported frame is bounded to 16 windows, 200 rows, 400 columns,
4,096 characters/16 KiB per source line, 2,048 visible text properties
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
exception handles `save`, `save_all` and `save_and_build` arriving with queued
input or transient Escape/keycode/mapping decoding: non-consuming `getchar(1)`
checks input availability, and the runtime queues one save until `SafeState` or
`SafeStateAgain`. It checks again before resuming, with a two-second deadline
and the originating current buffer recorded. Pending operators, command lines,
completion and incomplete counts/finds awaiting another key are rejected
immediately. Other native actions are rejected while a save waits or checks
input, except state requests and build/search cancellation. A changed current
buffer, timeout or disconnect clears
the queued save; it is never replayed later. Build validation occurs before
queuing a save-and-build and again before its writes execute.

Hosts must complete preceding queued PTY writes before sending a semantic save;
the runtime can only inspect input already delivered to Vim. Hosts can use
`Vim9Session::command_pending(id)` to hold later input until that specific
request leaves the pending set, without waiting for unrelated snapshot/render
requests. Pending IDs are registered before enqueue and cleared by responses
or disconnection. A false result indicates ordering completion, not successful
execution; failures and connection state still need normal UI handling.

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
supported Vim9 LSP plugin or optional workbench diagnostic hook. Duplicate
entries are removed. LSP's default UTF-16
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

The built-in adapter is [Yegappan's Vim9 LSP](https://github.com/yegappan/lsp).
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

### Simple plugin workbench

[beamiter/vimrc](https://github.com/beamiter/vimrc) can provide these optional,
already-loaded global functions. The runtime neither installs the vimrc nor
sources arbitrary project-local files:

```vim
def g:VimrcWorkbenchCapabilities(): list<string>
def g:VimrcWorkbenchAction(action: string)
def g:VimrcWorkbenchState(): dict<any>
def g:VimrcWorkbenchDiagnostics(): dict<any>
```

The first hook returns available actions for the **current** buffer. The bridge
accepts only `definition`, `hover`, `references`, `format` and the twenty-two
`simple_*` actions below, deduplicating at most 64 entries. Existing typed
semantic commands prefer a declared workbench provider, then retain their
built-in LSP/tags fallback. SimpleCC's rename uses its own prepare-rename and
Vim prompt workflow; it is deliberately a separate `simple_rename` action and
does not claim to consume the existing `rename` request's `name` argument.

| Fixed action | Plugin workflow |
| --- | --- |
| `simple_files` | SimpleFinder files |
| `simple_recent` | SimpleFinder recent files |
| `simple_tree_toggle` | SimpleRemote project tree, or SimpleTree when the remote wrapper is absent |
| `simple_tree_reveal` | Reveal the current file through SimpleRemote, or SimpleTree |
| `simple_terminal_toggle` | Show/hide the current SimpleTerminal session |
| `simple_terminal_new` | Open a new SimpleTerminal session |
| `simple_terminal_next` / `simple_terminal_previous` | Cycle SimpleTerminal sessions |
| `simple_buffers` | SimpleFinder buffers |
| `simple_grep` | SimpleFinder interactive project grep |
| `simple_outline` | SimpleCC symbol outline |
| `simple_rename` | SimpleCC rename with its Vim prompt |
| `simple_code_action` | SimpleCC code-action chooser |
| `simple_diagnostics` | SimpleCC current-buffer location list |
| `simple_next_diagnostic` | SimpleCC next visible diagnostic, including its severity filter and wrap behavior |
| `simple_previous_diagnostic` | SimpleCC previous visible diagnostic |
| `simple_git_status` | SimpleGit status |
| `simple_git_diff` | SimpleGit diff |
| `simple_git_hunk` | SimpleGit hunk preview |
| `simple_markdown_preview` | SimpleMarkdown preview |
| `simple_keys` | SimpleWhichKey hints |
| `simple_workbench` | Vimrc workbench report |

Every native command palette offers this finite catalog with aliases such as
`>simple`, `>simpletree`, `>simpleterminal`, `>git`, `>markdown` and `>workbench`. Absent plugins, disabled actions
and unsuitable buffer types remain visible with an unavailable reason. The
action hook must recheck availability and throw on failure. The native request
has no free-form Ex command, key sequence, callback name or executable field:

```json
{"id":42,"action":"workbench","command":"simple_git_hunk","window":1001,"buffer":3,"changedtick":7}
```

The bridge checks safe Vim mode, the exact current window/buffer/text version,
the fixed vocabulary and freshly sampled capabilities before invoking the
hook. It refuses a stale request instead of applying it to another source.
Plugin prompts, popup windows, diff views and other unsupported displays still
use the live compatibility renderer, returning to native paint when supported.
Before invoking a workbench hook, the bridge flushes a compatibility frame.
This makes blocking plugin input loops such as SimpleWhichKey's `getcharstr()`
visible even when they prevent paint timers from running. Native overlays stay
retired until the synchronous hook returns; failures also restore painting.
Asynchronous prompts and popups continue using the ordinary mode/display checks.

Tree actions reuse the vimrc's remote-aware commands when present, so a remote
workspace is not silently redirected to a local tree. Reveal requires a named
file buffer and follows the plugin's requirement to open the tree first;
it locates the file while preserving the source-window focus. Toggling the
tree also works from its own scratch buffer.
The companion SimpleTree change restores selection after a background file
refresh through `cursor()` rather than synthesized Normal keys. This keeps
Vim's input safety state intact, so a subsequent native action does not become
stuck behind a false pending-input state.
Terminal actions take no shell command or session identifier from the host.
The plugin owns shell selection, local/remote workspace routing and session
lifetime. Toggle or cycle with no existing session can create one, matching
the original plugin commands. Hiding a terminal preserves the running session.
Terminal-job mode continues to reject native workbench actions. Its F8 and
Shift-F8 keys stay with Vim in all four frontends, including when the native
Problems panel is visible; the same applies in Terminal-Normal mode.
Reopening a session can preserve Terminal-Normal mode; `i` resumes shell input
using Vim's existing terminal behavior.

`VimrcWorkbenchState()` must only read cached state. The bridge projects a
closed schema into `Vim9Snapshot.workbench`, ignoring all other fields:

```text
provider, root, native, large_file
git: head, ahead, behind, added, changed, removed
diagnostics: error, warning, info, hint
remote: connected, label, root
```

It attaches Vim-owned `buffer`, `window`, and `changedtick` fields, which the
receiver checks against the enclosing snapshot. String limits are 64
characters for provider, 128 for branch, 256 for remote label, and 4096 for
roots. Control characters are removed; integer counts are clamped to
0–1,000,000. Malformed optional fields use defaults. A missing, throwing or
disabled state hook yields empty context without interrupting editing. Private
plugin dictionaries, function references, channels and unrelated fields are
never serialized. `Vim9WorkbenchState::status_text()` supplies the four hosts'
compact palette/optional-status context from these caches, without Git/LSP I/O.

The bridge listens for `SimpleGitUpdate`, `SimpleCCDiagnosticsChanged`, remote workspace events and
`VimrcWorkbenchChanged`; its ordinary reconciliation also observes capability
changes after `:source`. The workbench status contains **severity counts** for
the current buffer. The optional diagnostic hook separately returns cached
problem rows without opening or replacing quickfix/location lists:

```text
{items: [{line: 1, column: 4, text: 'message', kind: 'warning'}], total: 1}
```

Positions are positive, one-based line and UTF-8 byte column numbers in the
current buffer. The bridge supplies the filename and accepts only those four
item fields, at most 256 rows and 2048 characters per message. Invalid rows are
ignored; a bounded `total` carries provider omissions into the existing shared
diagnostic budget. Rows are deduplicated with quickfix/location/LSP results.
Missing, disabled or throwing hooks expire old provider rows. A changed cached
snapshot is detected even without a User event or buffer edit.

The vimrc adapter calls only an already-loaded `simplecc#DiagnosticSnapshot()`.
SimpleCC converts its UTF-16 positions using loaded buffer text and memoizes
the last bounded result by buffer, text version, diagnostic version, source
filter and diagnostic path. It performs no daemon request or list mutation.
Counts and rows can differ when diagnostics have invalid positions/messages.
Simple diagnostic navigation uses the plugin's own filters; the existing
`next_diagnostic`/`previous_diagnostic` actions still traverse Vim's quickfix list.

## Real-editor tests

From the core repository:

```sh
cd runtime/vim9
VIM9_TEST_VIM=/absolute/path/to/vim python3 -m unittest -v test_bridge test_workbench test_render_windows test_render_matches test_render_delta
```

The optional cross-repository suite uses actual `vimrc`, `simplecc`,
`simplegit` and `simplewhichkey` checkouts. It defaults to sibling directories;
other checkout layouts can select the vimrc and plugin parent explicitly:

```sh
VIM9_TEST_VIMRC=/path/to/vimrc VIM9_TEST_SIMPLE_ROOT=/path/to/plugin-parent python3 -m unittest -v test_vimrc_workbench test_simple_diagnostics test_simple_finder test_simple_workspace
```

That suite loads the real configuration and plugin runtimes in isolated test
homes. SimpleCC's deterministic daemon fixture supplies replies; it tests
actual command routing, navigation, diagnostic caches, report rendering and
reload behavior, plus prompt acceptance/cancellation, blocking WhichKey input,
popup-to-native recovery and diagnostic publication/clearing with Unicode byte
positions. These tests run without installing plugins or contacting language servers or
remote machines. Its fixture replies are not evidence of an external LSP
server's correctness.
The optional finder/workspace suites additionally need the SimpleFinder,
SimpleTree and SimpleTerminal checkouts. They exercise real buffer selection,
preview cancellation, tree toggle/reveal and a local `/bin/sh` terminal,
including F8 return and session reuse. Tree tests use an existing daemon binary
or the plugin's checked-in protocol fixture; nothing is downloaded.

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

The window suite adds same/different-buffer splits, independent scrolling,
Visual-mode sampling without focus/autocommand side effects, multiline syntax
invalidation and unsupported-peer recovery. The delta suite uses the actual
Vim channel to verify cursor-only updates, Unicode edit/save, explicit cleared
rows, sibling-pane preservation, forced checkpoints and safe resynchronization.
Rust tests additionally inject fragmented channel messages and missing bases
to verify automatic recovery without losing editing commands.

These tests do not validate frontend font rendering, desktop IME integration,
mouse input or all user plugins; those are frontend/desktop verification tasks.

## Scroll performance

`python3 runtime/vim9/benchmark_scroll.py --lines 10000` measures protocol-5
source-bound scroll requests with real Vim, Rust syntax, Unicode, relative
numbers, CursorLine and one/two windows. `--runtime /path/to/bridge.vim` compares
an exported baseline. It reports ordered semantic ack/response latency and Vim
CPU, not GUI FPS. See the [measured pipeline follow-up](../../docs/vim9-scroll-validation.md)
for the workload, results and remaining display acceptance boundaries.

## Primary references

- [Official Vim channel/job documentation](https://github.com/vim/vim/blob/master/runtime/doc/channel.txt): NL framing, callbacks, async jobs and channel lifetime.
- [Official Vim9 documentation](https://github.com/vim/vim/blob/master/runtime/doc/vim9.txt): compiled `def` functions and coexistence with legacy user scripts.
- [Official Vim builtin documentation](https://github.com/vim/vim/blob/master/runtime/doc/builtin.txt): buffer snapshots, quickfix, state checks, filename escaping and view preservation.
- [Official Vim autocommand documentation](https://github.com/vim/vim/blob/master/runtime/doc/autocmd.txt): mode/text/quickfix events and pending-state behavior.
- [Official Vim startup documentation](https://github.com/vim/vim/blob/master/runtime/doc/starting.txt): normal vimrc/plugin loading and the `-S` startup hook.
- [Vim9 LSP public documentation](https://github.com/yegappan/lsp/blob/main/doc/lsp.txt): supported semantic commands, diagnostics and user events.

# Native Vim9 workbenches

Anvil, Forge, Ember and Frost share an editor bridge around a **real Vim 9
process**. Managed shell entry stays in the originating GUI: Ember uses an
editor tab, while Anvil, Forge and Frost embed the workbench in the source pane.
Explicit standalone IDE launches remain available. Each can subscribe to native
GUI buffer paint frames independently of Vim's PTY compatibility display.
The workbench supplies project navigation, buffer tabs, code actions, project
search, build controls, quickfix navigation and editor state.

Vim owns the text, modes, undo tree, registers, macros, splits, completion and
plugins. Native controls send typed actions; they do not type Ex commands into
an arbitrary shell or emulate Vim editing. For supported buffer views, the
center editor draws buffer-origin text with Vim's **GUI** highlight colors and
attributes through native GUI text rendering. PTY input remains connected to
real Vim; the GUI does not maintain another editable document.

## Native buffer display

The core publishes an immutable `Vim9RenderFrame` over the authenticated editor
channel after the host opts in with `Vim9Command::SetRender { enabled: true }`.
The renderer receives UTF-8 text, Vim display positions, GUI `guifg`/`guibg`/
`guisp`, bold, italic, underline, undercurl, strike and reverse attributes.
These colors do not come from ANSI output, terminal cell colors or `ctermfg`.
This also works with `notermguicolors`: hexadecimal and named GUI colors,
including linked highlight groups, retain their GUI values.

Supported ordinary code windows, including horizontal, vertical and mixed Vim
splits, include editing, normal/insert/
replace modes, character and line selections, tabs, CJK/wide characters,
base-plus-combining text, number/relative-number gutters, CursorLine, single-line
search highlights and non-virtual diagnostic text-property highlights. A
standard single sign column renders the highest-priority visible sign text,
GUI sign color, line/number highlights and equal-priority combinations. A
non-ASCII grapheme keeps its own Vim display advance so font fallback cannot
shift later glyphs or the input cursor. The host paints only the published
window rectangles independently; command/status rows and split separators
between them remain Vim-owned. Each run names its actual Vim window, buffer
line and byte range; each window carries its own buffer tick and viewport.
The active cursor and Visual selection cannot leak into sibling windows.

This is an incremental GUI buffer renderer, not a complete external Vim UI
protocol. Special buffers, actual wrapped continuation rows, closed
folds, visible popups/completion, multiple/image sign columns, fold columns, virtual text, matchadd
decorations, multiline/positional/zero-width search, virtual-space cursors,
list/conceal/spell/right-to-left display and block selections through tabs currently publish an explicit
compatibility reason. Complex zero-width/bidirectional text and clipped wide
glyphs also fall back. Vim's editing options and text are not changed to make a
frame renderable. More than 16 windows or an unsupported window causes a
complete compatibility frame. A host must show the live Vim compatibility surface whenever
the frame is unsupported, disconnected, stale or has mismatched geometry.

Input-driven paints are coalesced on a separate 16 ms timer, independently of
the 40 ms metadata timer. Visible glyph/syntax data is cached by buffer tick,
viewport, theme, search and property state. Each window keeps only its visible
line cache, so scrolling reuses surviving lines without repeating their syntax
queries. Any buffer tick invalidates that cache, including multiline syntax
effects beyond an edit. Idle reconciliation catches direct `:highlight`
changes; command-mode transitions and `Syntax`/`FileType` invalidate rules.
An asynchronous plugin changing only syntax rules must emit
`doautocmd <nomodeline> User JTermVim9RenderInvalidate`.
Identical paint frames are not republished.
`render_snapshot()` clones only an `Arc`, without copying buffer lists,
diagnostics or build output on each GUI frame. Paint work and wire frames are
bounded; oversized views/lines fall back to Vim. These timing choices are not
an end-to-end latency or frame-rate guarantee.

The authenticated host negotiates `render_protocol: 2` for real row deltas.
`render_delta` carries an exact `base_revision`, the new frame metadata and
`row_updates`. Each item replaces all paint runs on one absolute screen row,
including every split on that row; an empty list clears old glyphs. A cursor
move can therefore publish no text. Layout/theme/compatibility changes,
resynchronization and periodic checkpoints use complete `render` keyframes.
Older hosts continue receiving full frames. The Rust receiver validates
nonoverlapping window ownership and applies a delta atomically. A lost or
invalid base immediately hides native paint and requests a fresh keyframe,
while Vim editing, saving and metadata communication remain available.

See the [architecture audit and measured validation](vim9-native-render-audit.md)
for stock Vim API limits, the frontend matrix, performance observations and
remaining OS IME/Wayland verification gaps.

## Launch

Install Vim 9 with `+vim9script`, `+eval`, `+channel`, `+job`, `+timers` and
`+quickfix`. The workbench loads
the user's normal vimrc and plugins and then sources its bundled Vim9 runtime.
Use the native **Vim9 IDE** entry or launch a project directly:

```sh
anvil --ide ~/projects/jwm
forge --ide ~/projects/jwm
ember --ide ~/projects/jwm
frost --ide ~/projects/jwm
```

`--ide` without a path uses the current directory. A file target opens that file;
the core searches its parent directories for a project marker such as `.git`,
`Cargo.toml`, `package.json`, `pyproject.toml`, `CMakeLists.txt` or `Makefile`.
An explicit directory target is used as the project root without climbing.
New files in an existing directory can also be opened. Workbench paths must be
UTF-8. `JTERM_VIM9_BIN=/absolute/path/to/vim` selects another Vim executable.

Normal terminal startup remains available without `--ide`. Opening an IDE from
an existing terminal creates a dedicated editor; it does not commandeer the
foreground shell. Editor bridge credentials and direct editor commands are
ephemeral and must not be saved as restorable shell sessions.
Anvil, Forge and Frost open a separate workbench process, so closing the terminal
window that launched it does not terminate the editor. Ember creates
an editor tab in its existing terminal application.

## Edit, navigate, search and build

Open files from the native file controls. The buffer strip reflects Vim's
listed buffers and marks modified buffers. Clicking a problem or search result
opens its real filename and line/column. Save, Save All, buffer changes and
diagnostic navigation retain Vim's ordinary errors and modified-buffer checks.
Vim commands such as `:vsplit`, `:tabnew`, `:undolist` and your existing mappings
continue to work in the editor.

The four workbenches share these editor shortcuts (active in the IDE editor,
not in a normal terminal tab or a native text field):

| Shortcut | Action |
| --- | --- |
| `Ctrl+Shift+P` | Quick Open: fuzzy-match project-relative filenames |
| `Ctrl+Shift+G` | Go to a one-based `line` or `line:byte-column` in the current buffer |
| `Ctrl+S` / `Ctrl+Shift+S` | Save / Save all |
| `Ctrl+Shift+F` | Focus project search |
| `Ctrl+Shift+B` | Focus the build command; enter or confirm to run |
| `F8` / `Shift+F8` | Next / previous quickfix problem |
| `Ctrl+Shift+PageUp` / `Ctrl+Shift+PageDown` | Previous / next buffer |
| `Ctrl+Shift+W` | Close the current buffer, refusing unsaved changes |
| `Alt+Left` / `Alt+Right` | Back / forward through the current Vim window's jump history |

Quick Open uses an asynchronous `rg --files` index shared by all frontends.
It includes hidden files, respects `.gitignore` and `.ignore`, excludes `.git`,
and does not follow directory symlinks. Type part of a filename or path, move
with the arrow keys, and press Enter to open the selected result; Escape
returns to Vim. Refresh after creating files outside the workbench. Scanning
is bounded to 20,000 files, 16 MiB of retained path text and 15 seconds;
partial results and failures are reported. No recursive I/O runs in a render
callback, snapshots share immutable file lists, and ranking is recalculated
only when the query or index changes. Install ripgrep for Quick Open.

Native split controls create real Vim horizontal/vertical windows sharing
Vim's buffers and undo. Buffer navigation restores remembered cursor/scroll
positions. Closing a buffer never forces a write or discards changes, even
when `hidden` is set; closing the last saved buffer leaves an unnamed Vim
buffer. Keyboard close resolves the current buffer inside Vim after earlier
navigation commands; a buffer tab's close button targets that specific tab.
Ordinary `Ctrl+P`, `Ctrl+G`, `Ctrl+O` and `Ctrl+W` remain Vim keys.

The native Back and Forward controls share Vim's real per-window jumplist
with `Ctrl+O` / `Ctrl+I`, including file, definition and quickfix jumps. Native
Go to line and result clicks inside the current file record their origin as
well. Buttons reflect whether earlier/later positions are available; a split
has its own Vim history. Navigation uses ordinary modified-buffer checks and
refuses a pending operator or command line. Alt-arrow shortcuts only apply
while the editor owns input; native search and path fields retain their keys.

If Save arrives while Vim is decoding an Escape or mapping prefix, the bridge
waits for Vim's `SafeState` event for up to two seconds and shows a waiting
status. It preserves the input already typed and never inserts Escape on the
user's behalf. A pending operator, command line or completion is still refused.
The queued save stays bound to its original buffer; changing that buffer or
exceeding the timeout produces an error and requires another explicit save.
Other native editing actions cannot overtake a waiting save.

Project search uses `rg` and populates quickfix results. Its defaults remain
literal text and Smart case: lowercase queries ignore case, while a query
containing uppercase matches case. The native search options offer:

| Option | Meaning |
| --- | --- |
| Smart / Match / Ignore case | Choose automatic, case-sensitive or case-insensitive matching |
| Regex | Interpret the query using ripgrep's regular-expression syntax |
| Whole word | Require word boundaries around a match |
| File glob | One optional glob, such as `*.rs`, `src/**` or `!*.lock` |
| Stop search | Cancel the running search and keep the previous result list |

A glob is one value; spaces and commas are preserved, and shell expansion is
not performed. Changing options does not start a job until Search is invoked.
Queries and globs must be single-line strings of at most 4096 UTF-8 bytes.
The workbench shows the options used by the last job, its progress, errors,
retained matching-line count and omitted matching lines. Install ripgrep for
this action. Query and glob values are passed as argv arguments; project ignore
files apply and `.git` remains excluded. `RIPGREP_CONFIG_PATH` is disabled for
these managed searches so a global rg configuration cannot override the
selected options or change the result format.

A cancelled search retains the previous quickfix list. Its late subprocess
callbacks cannot publish partial results or overwrite a later search. An
invalid regular expression is shown as a search error and also retains the
previous list; a successful search with zero matches clears it.

Build runs the command explicitly entered in the workbench asynchronously using
Vim's shell configuration and parses output with Vim's `errorformat`. An empty
build field uses Vim's `makeprg`. Build never saves buffers implicitly. For
example, use `cargo check --message-format=short`, `make`, or a project's test
command. Select the corresponding Vim `:compiler` or configure `errorformat`
when a tool's output format differs. The editor remains usable while the job
runs; its running state and exit code are shown, and Stop Build requests
cancellation. No project build is run automatically just by opening a folder.

The separate **Save all + Build** action explicitly saves first. Invalid
commands, an existing build, write errors or remaining modified buffers prevent
the new build from starting. `:wall` can still save some buffers before a later
buffer reports an error; this is Vim's ordinary save-all behavior. The plain
Build action continues to leave in-memory edits unsaved.

Quickfix is the common problem/result surface: a later build or search replaces
the current list according to the runtime workflow. The native panel identifies
the current quickfix list's source, title and position separately from the last
search status. Its diagnostics can additionally include Vim location-list and
LSP entries. Each search list owns its omitted count, so starting, failing or
cancelling a different search does not change the retained list's count.
Build directory metadata is removed after parsing, without appearing as a
spurious quickfix item. At most 256 buffers and
256 diagnostic entries are sent to native controls. Project search retains
at most 256 matches in quickfix and reports omissions. Build captures at most
10,000 output lines for quickfix parsing and independently keeps the latest
80 output lines for the native workbench, including when quickfix capture is
full. The omitted-line count describes output not present in that native tail;
cancelled builds are shown distinctly from successful completion.
Other Vim buffers and quickfix lists remain owned by
Vim. Project directory navigation is one level at a time and bounded
to 1024 directory entries. The workbench does not recursively scan a whole
repository on the render path.

## Language features

Definition, hover, references, rename and formatting buttons are enabled only
when the runtime finds a supported Vim plugin command. Definition also supports
Vim's native tags or configured `tagfunc`. Language servers remain
configured by the user's Vim installation. The workbench does not silently
download plugins or language servers, and it does not claim semantic features
for a bare Vim installation. See the runtime's [protocol and adapter notes](../runtime/vim9/README.md)
for supported commands and optional setup.

## Closing and lifetime

Native editor close requests ask Vim to quit without `!`. Unsaved changes can
therefore keep the editor open; save them or explicitly use Vim's `:qa!` when
discarding is intended. A socket disconnect is not permission to kill a Vim
process. Frontends own PTY lifetime and should display editor/bridge errors
without replacing the current file or starting another editor automatically.

Each workbench has its own random loopback endpoint and token. The runtime and
host authenticate each other before accepting actions. The runtime unsets
the bootstrap token environment variable before starting build jobs and clears
its script-local token after authentication. There is no
network listener on an external interface and no general remote `eval` or Ex
command API. User-entered build commands deliberately retain shell semantics.

## Host integration

```rust,ignore
use jterm_core::vim9::{Vim9Session, Vim9Command};

let editor = Vim9Session::start(project_path)?;
// Spawn editor.launch() in a new PTY with its cwd and child-local environment.
// Existing argv-only adapters may spawn editor.launch_argv() instead.
let state = editor.snapshot(); // A bounded clone; socket I/O is on a worker.
if state.connected {
    editor.send(Vim9Command::Open {
        path: "src/main.rs".into(), line: Some(20), column: Some(1),
    })?;
}
```

Keep the controller associated with a stable editor/PTY identity, rather than
the current numeric tab index. The last controller drop disconnects its worker
without blocking a UI thread. Commands are bounded to 64 KiB, incoming frames
to 1 MiB, and outstanding requests to 64. Commands are never replayed into a
replacement editor after disconnection. Errors are visible in the snapshot;
`clear_error()` dismisses them.

For a native Quick Open picker, create `Vim9ProjectIndex::start(editor.root())`
when the picker is first requested. `snapshot()` cheaply clones an immutable
`Arc<Vec<Vim9ProjectFile>>`; call `snapshot.matches(query, result_limit)` only
when the query or that Arc changes. Display scanning, truncation and errors,
and send the selected file's absolute `path` using `Vim9Command::Open`.
Dropping the last index handle cancels its worker without joining the UI
thread. Replacing the handle refreshes the project index.

## Verification

`cargo test vim9::tests` exercises real loopback connections, authentication,
fragmented input, bounded queues/frames, editor-state synchronization and
disconnection. The runtime directory includes an integration test that starts
an actual PTY Vim process and drives its native channel and editing behavior.
The ignored `vim9::tests::real_vim9_round_trip` test additionally checks the
complete typed Rust controller against real Vim: launch, open a special
filename, edit, refuse an unsaved quit, save, parse a failed build, jump,
cycle buffers, split, save-and-build, close a buffer and quit.
The Python suite also exercises immediate Escape/save sequencing, bounded
deferred-save expiry without replay, and consecutive navigation/current-close
requests without a native snapshot barrier. Its navigation/search regressions
exercise native and keyboard jumplist interoperability, per-window history,
unsaved-buffer refusal, every case mode, regex, whole words, literal globs,
global-rg-config isolation, cancellation with late output, invalid regexes and
the identity/omission counts of retained quickfix lists.
Run it with `VIM9_TEST_VIM=/absolute/path/to/vim cargo test --lib
vim9::tests::real_vim9_round_trip -- --ignored` in an environment with a PTY
and loopback sockets. See the runtime README for the Python suite's Vim
executable and runtime environment variables.
The frontend workbenches additionally require native window/keyboard checks
on the desktop where they will be used; core tests alone do not verify GPU,
Wayland, font shaping, IME or a particular user's plugin configuration.

Official references:

- [Vim9 script](https://vimhelp.org/vim9.txt.html)
- [Vim channels, jobs and newline framing](https://vimhelp.org/channel.txt.html)
- [Vim quickfix and compiler integration](https://vimhelp.org/quickfix.txt.html)

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
base-plus-combining text, number/relative-number gutters, CursorLine,
CursorColumn and colorcolumn guides, single-line
search highlights, window-local plugin matches, diagnostic text-property
highlights and bounded after/EOL virtual labels (see below). A
standard single sign column renders the highest-priority visible sign text,
GUI sign color, line/number highlights and equal-priority combinations. A
non-ASCII grapheme keeps its own Vim display advance so font fallback cannot
shift later glyphs or the input cursor. The host paints only the published
window rectangles independently; command/status rows and split separators
between them remain Vim-owned. Each run names its actual Vim window, buffer
line and byte range; each window carries its own buffer tick and viewport.
The active cursor and Visual selection cannot leak into sibling windows.

This is an incremental GUI buffer renderer, not a complete external Vim UI
protocol. Terminal and prompt buffers, actual wrapped continuation rows, closed
folds, general popup windows and complex/scrolling completion menus, multiple/image sign columns, fold columns, inline/aligned/wrapping virtual text,
multiline/positional/zero-width search and match patterns, conceal matches, virtual-space cursors,
unsupported listchars, conceal/spell/right-to-left display and block selections through tabs currently publish an explicit
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

Ordinary single-line `matchadd()` / `:match` patterns and byte-based
`matchaddpos()` decorations use native GUI styles, including Vim's built-in
`matchparen` plugin. Matches remain window-local. The highest-priority match
wins; equal priorities follow insertion order, and search wins priority-zero
ties. Match patterns retain Vim's case-sensitive/magic defaults independently
of search options. Syntax and diagnostic properties combine below the winning
match, with Visual selection above it. Timer-driven match additions/deletions
invalidate paint without editing the buffer or requiring a provider hook.
The bridge bounds each window to 64 matches and 2048 positions and each source
line to 512 match/search ranges. Unsupported patterns or exhausted budgets
publish compatibility frames instead of truncating highlights.
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

Render protocol 4 retains version 3's source-bound input and publishes up to
eight real source rows before/after each viewport. Version 3 hosts continue
receiving two rows. The wider bounded margins let ordinary three-line wheel
notches and short bursts move through real text while awaiting Vim's exact
acknowledgement. Oversized frames still retire margins before falling back.

Discrete wheel input follows a 100 ms time-based transition shared by all four
frontends. Continuous notches add independent, finite transitions; reversing
retires the previous unplayed tail. Trackpad pixels remain direct OS input.
The GUI frame clock drives the transition independently of Vim metadata or
new render messages, and pointer capture freezes future wheel animation at
the source image used for hit testing. Editing, zoom and topology changes
retain their existing cancellation rules.

See the [architecture audit and measured validation](vim9-native-render-audit.md)
for stock Vim API limits, the frontend matrix, performance observations and
remaining OS IME/Wayland verification gaps.

## Scratch buffers and window-local colors

Ordinary text buffers with `buftype=nofile`, `nowrite` or `acwrite` use native
text rendering and source-bound mouse/scroll input under the same display and
input eligibility rules as files. Their source remains Vim's real buffer,
including its undo history, `modifiable`, `readonly` and plugin mappings.
The bridge does not turn a scratch name into a filesystem write: `:update` is
Vim's normal no-op for unmodified nofile/nowrite buffers, while acwrite retains
its `BufWriteCmd` callback and reports Vim's error if no handler is available.
Terminal and prompt buffers retain the compatibility surface. Help and error-list
windows use the separate native-paint/Vim-input policy below.

`wincolor` keeps each split's Normal colors and attributes on the native path:

```vim
highlight NativePane guifg=#ddeeff guibg=#17212b
setlocal wincolor=NativePane
```

The window background covers empty lines, end-of-line space, end-of-buffer
rows and scroll margins. Syntax, signs, line numbers and selections keep their
own highlight precedence. GUI caret fallback, input-method preedit and focus
indicators use the corresponding window palette; retained scroll imagery is
retired when that palette changes. Global `fg`/`bg` highlight references remain
global. Vim versions with `winhighlight` also accept its single Normal mapping;
more general window highlight remapping still uses compatibility rendering.

## Help and error lists

Help, quickfix and location-list windows can use native text rendering when
their visible content passes the normal display checks. QuickFixLine follows
the actual current entry of the corresponding Vim list, independently of the
text cursor and buffer changedtick. Global quickfix and per-window location
lists retain separate highlights. Existing syntax, column and Visual layers
keep Vim's priority.

These special windows retain Vim's own PTY keyboard, mouse and wheel path.
Enter, Ctrl-], double-clicks and plugin mouse mappings are not translated into
source-editing commands, and the host does not add scroll animation to them.
Normal file/scratch splits beside them retain native precision input. A switch
from a fractional file view first retires its old presentation before handing
raw pointer coordinates to the special window.

The bridge does not change display options to make a view native. Default help
concealment and actual wrapped continuation rows still use compatibility
rendering. A user who chooses plain help text can use
`setlocal conceallevel=0 nowrap`; help remains readonly/nomodifiable according
to Vim's own options.

## Native column guides

`cursorcolumn` and `colorcolumn` now stay on the native GUI path in supported
ordinary buffer views. Each Vim window keeps its own options and cursor column.
Absolute columns and signed columns relative to `textwidth` are supported:

```vim
set cursorcolumn
set colorcolumn=80,120
" Relative columns use the current textwidth:
set textwidth=100 colorcolumn=+1
```

The producer composes these highlights with syntax, CursorLine, matches,
search, text properties and Visual selection. Wide characters keep their
whole source range, while an expanded tab may show a highlighted column
without changing its original byte range. Empty-line and end-of-line guides
are display-only decorations; they do not become editable text or mouse
targets. `cursorlineopt=both` also retains its line highlight. Existing wrapped
line, closed-fold and other compatibility boundaries remain in effect.

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
listed buffers and marks modified buffers. When the bounded list exceeds 256
items, the current listed buffer is retained so its tab and modified state
remain visible. Byte-budget trimming also prefers to retain it, provided its
own entry fits the frame limit; omitted buffers remain available through Vim. Clicking a problem or search result
opens its real filename and line/column. Save, Save All, buffer changes and
diagnostic navigation retain Vim's ordinary errors and modified-buffer checks.
Vim commands such as `:vsplit`, `:tabnew`, `:undolist` and your existing mappings
continue to work in the editor.

The four workbenches share these editor shortcuts (active in the IDE editor,
not in a normal terminal tab or a native text field):

| Shortcut | Action |
| --- | --- |
| `Ctrl+Shift+P` | Quick Open: fuzzy-match filenames, or start with `>` for commands |
| `Ctrl+Shift+G` | Go to a one-based `line` or `line:byte-column` in the current buffer |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | Increase / decrease / reset editor font size |
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

Quick Open also accepts `path:line[:byte-column]`, including fuzzy paths such as
`src/mrs:42:8` and indexed absolute paths copied from a diagnostic. Enter
`:42` or `:42:8` to move in the current buffer, including an unnamed or modified
buffer; this does not require a successful file index. The picker shows the
destination, validates positive coordinates up to 2147483647, and rechecks the
current query before opening. The column is a one-based UTF-8 byte column, as
in Vim and compiler diagnostics. Invalid input keeps the picker open and cannot
activate a previous result. Queries are bounded to 256 characters.

Exact indexed filenames containing colons take precedence over position syntax.
An exact indexed prefix also wins: when `notes:42` is a real file,
`notes:42:9` opens that file at line 9. Unindexed/external files still use the
explicit Open path control. All four frontends share the same parser and send
typed Open/Goto actions; the query never becomes an Ex or shell command.

Start the same picker query with `>` to search the shared command palette.
For example, `>vertical split`, `>format` and `>hover` find split, formatting
and symbol documentation actions. Names and aliases support case-insensitive
fuzzy matching, including common Chinese labels. The palette lists shortcuts
and explains unavailable actions, such as a missing language-service capability,
an empty quickfix list or a stopped build. Availability is checked again on
activation against the current Vim state. Save/all, buffer switching, splits,
jump history, quickfix navigation, definition, documentation, references,
formatting and stopping an active build/search use existing typed commands;
the picker does not execute arbitrary Ex or shell text. Commands work without
the project index, including while scanning fails. Use `./>notes` to find a
literal filename starting with `>`. Escape returns focus to the editor.

When using `beamiter/vimrc`, `>simple` exposes its SimpleFinder, SimpleCC,
SimpleGit, SimpleMarkdown, SimpleWhichKey, SimpleTree and SimpleTerminal workflows. `>git`, `>markdown`
and `>workbench` narrow that catalog. Missing plugins or unsuitable buffers
show an unavailable reason, and activation checks the actual source window,
buffer and text version before calling the vimrc's fixed action adapter.
The command palette also displays cached branch/hunk counts, diagnostic
severity counts and connected remote workspace context; the same compact
summary appears in the optional statusbar. No permanent panel is required.
See [the Simple workbench contract](../runtime/vim9/README.md#simple-plugin-workbench)
for hook names, limits and compatibility behavior.

`>simpletree` toggles the project tree or reveals the current file using the
same remote-aware routing as the vimrc's tree mappings. `>simpleterminal`
shows/hides, creates or cycles plugin-owned sessions. The plugin can create a
session when none exists and follows the current local/remote workspace.
F8/Shift-F8 remain Vim keys while a terminal is active, even with the native
Problems panel visible. Hiding it returns to the editor without killing the
shell. `Simple: Find recent files` adds the existing SimpleFinder history picker.

Native split controls create real Vim horizontal/vertical windows sharing
Vim's buffers and undo. Buffer navigation restores remembered cursor/scroll
positions. Closing a buffer never forces a write or discards changes, even
when `hidden` is set; closing the last saved buffer leaves an unnamed Vim
buffer. Keyboard close resolves the current buffer inside Vim after earlier
navigation commands; a buffer tab's close button targets that specific tab.
Ordinary `Ctrl+P`, `Ctrl+G`, `Ctrl+O` and `Ctrl+W` remain Vim keys.

When multiple native splits are visible, a thin inner accent outline identifies
the active Vim window without moving text or covering its status line or
separators. Completion overlays remain above the focus indicator. Problem rows
show severity and a project-relative location, with full detail available on
hover; the actual quickfix list continues to own F8 navigation.

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

The vimrc workbench adapter supplies SimpleCC definition, hover, references
and formatting through the same native controls. `Simple: Rename symbol with
Vim prompt` preserves SimpleCC's own prepare-rename/prompt workflow; the
existing typed Rename dialog still requires a provider accepting its supplied
name. Plugin popup and diff workflows can temporarily use the live Vim
compatibility view. Native painting resumes when the current display is
supported again.

SimpleCC's cached current-buffer diagnostics also populate the native Problems
rows without first opening a Vim location list. Their columns are converted
from UTF-16 to Vim's UTF-8 byte positions, including Chinese text and emoji.
Diagnostic publication and clearing trigger a bridge refresh; selecting a
different buffer selects that buffer's cache. The command palette offers
`Simple: Show buffer diagnostics`, `Simple: Go to next buffer diagnostic` and
`Simple: Go to previous buffer diagnostic` (search `>simple diagnostic`). These
actions use SimpleCC's own list, filtering and navigation semantics.

Blocking workbench plugins such as SimpleWhichKey get the live Vim surface
before their input loop begins. The native overlay returns after the hook
finishes or fails. Real-plugin regression tests cover key hints, rename
accept/cancel, code-action cancel and subsequent typing in the original buffer.

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

The Simple diagnostics/input follow-up was validated with 76 related real-Vim
tests (bridge, workbench, actual vimrc/Simple plugins, row deltas and split
windows), 110 shared Rust Vim9 tests, SimpleCC's 15 Vim suites and Vimrc's
workbench regression. The cross-repository tests use isolated homes and a
deterministic daemon fixture, including direct cache reads, Unicode navigation,
publication/clearing and real plugin prompts. Rust formatting and all-target
Clippy passed. The subsequent workspace follow-up adds real Finder selection,
tree reveal/background refresh and two-session terminal tests, plus 111 shared
Rust Vim9 tests. F8/Shift-F8 routing tests passed for all four frontends; the
GTK harnesses use the matching Nix GCC/glibc toolchain. The initial GTK SDK gap
was resolved with its exact `sysprof-capture` and `appstream` development
dependencies, allowing Forge and Anvil's all-target checks to run as well.
The local-family runner itself does not change release pins or install
binaries. Desktop GPU/IME acceptance remains separate from these checks.

Official references:

- [Vim9 script](https://vimhelp.org/vim9.txt.html)
- [Vim channels, jobs and newline framing](https://vimhelp.org/channel.txt.html)
- [Vim quickfix and compiler integration](https://vimhelp.org/quickfix.txt.html)


## Vim-owned native cursor

Supported buffer frames publish an optional `cursor.appearance` from the real
Vim `guicursor` setting. Normal, Visual (including exclusive selection),
operator-pending, Insert and Replace resolve their own block/vertical/horizontal
shape and percentage. Ordered `a:` overlays, linked highlight groups and the
second language-mapping highlight are supported without changing Vim options.
A changed highlight or cursor option can travel in a cursor-only row delta.
Older frames without appearance keep the frontend's compatibility cursor.

The shared `Vim9CursorBlink` follows `blinkwait`, then `blinkoff`/`blinkon`;
any zero timing disables blinking. Real source/mode/focus changes and accepted
keyboard/IME activity restart the wait; unrelated paint revisions do not.
Unfocused cursors stay visible as outlines and request no blink wakeups.
The small shape's paint rectangle never replaces the full wide-character
source/IME rectangle. Solid cursors repaint the source glyph in the cursor
foreground, retaining shaping and font fallback. With no explicit cursor
colors, the underlying syntax/Visual colors are reversed.

For example, keep a solid normal-mode block, an insertion bar and a replacement
underline, with different GUI colors supplied by the user's highlight groups:

```vim
highlight Cursor guifg=#20242c guibg=#d8dee9
highlight InsertCursor guifg=#20242c guibg=#73a2e1
set guicursor=n-v:block-Cursor,i:ver25-InsertCursor,r:hor20-Cursor,a:blinkon0
```

Command lines, completion popups and other unsupported displays continue to use
Vim's live compatibility surface. Native showmatch timing is not inferred from
terminal bytes. The runtime tests exercise real modes, ordered overlays,
exclusive Visual inheritance, language mappings, linked/theme RGB changes,
wide/combining/empty source cells and cursor-only deltas.


## Native Insert completion (render protocol 5)

Ordinary Insert completion can keep the native GUI buffer visible. The bridge
reads the real `complete_info()` candidates and `pum_getpos()` geometry, then
publishes a separate optional `completion` object with `view`, `default_style`
and display-only `runs`. The rectangle includes Vim's one-cell left padding
where present; it may cross splits and Vim status rows. Completion runs never
become buffer/source anchors. GUI `Pmenu`, `PmenuSel`, `PmenuKind[Sel]` and
`PmenuExtra[Sel]` styles are independent of terminal `cterm` colors. Available
per-item `abbr_hlgroup`/`kind_hlgroup` metadata is merged when Vim exposes it.
Older Vim builds (including the tested 9.2.0858) omit those fields from
`complete_info()`; on them only the standard menu highlight groups are known.

Candidate generation, filtering, key mappings, selection, acceptance and
cancellation stay in Vim. The menu uses Vim's exact grid and original PTY
keyboard/mouse route, without stealing focus or adding a second completion
engine. Menu appearance retires old native scroll/pointer transforms.
`CompleteChanged` and `CompleteDone` request a native paint; selection-only
and close updates can be row deltas, and `completion: null` clears old paint.
The receiver validates menu geometry, GUI colors, budgets and source-free runs.

The supported list must fit completely without a scrollbar and use default
column order, fully visible ordinary labels, and non-fuzzy ordering. Scrolling,
filtered, clipped, bordered, translucent, custom `pumopt`, unregistered info/popup
windows and distinct `PmenuMatch[Sel]` highlights retain explicit compatibility
reasons. Public Vim APIs do not expose the first visible candidate or original
match leader; this implementation does not guess them. Protocols 1–4 always
retain the existing compatibility behavior while a completion menu is visible.

## Native Simple text popups (render protocol 6)

SimpleCC hover, signature, diagnostic and completion-documentation windows, and
SimpleGit hunk/blame windows can retain native buffer painting. Each audited
provider marks its passive, opaque, mask-free popup with
`setwinvar(id, 'jterm_native_popup', 1)` after content and filetype are ready.
It then emits `User JTermVim9PopupChanged` when a subscriber exists, scheduling
the next native frame without a document edit or slow polling round trip.
The marker is a provider contract: remove it before introducing masks or
nonrectangular content. Vim's public `popup_getoptions()` does not expose masks
reliably, so unregistered popups always use compatibility rendering.

The bridge samples popup buffer text, syntax/text-property GUI styles and
`popup_getpos()` geometry through Vim APIs; it does not scrape terminal cells.
`popups` is a complete list of `{id, zindex, view, default_style, runs}` surfaces
ordered by ascending z-index then descending ID: Vim places the older popup
on top when z-indices match. Body, padding, border and Unicode glyphs are explicit
paint. Popup runs have zero `window`, `line`, `byte_start` and `byte_end`: they
are never source anchors. Limits are eight popups, 64 by 512 cells each, and
4,096 popup runs per frame. The receiver rejects malformed geometry, colors,
duplicate IDs, wrong order and source-bearing runs atomically.

All four hosts paint popups below z-index 100 before the ordinary completion
menu and the remaining popups after it, across split/status gaps and above the
native cursor. An empty `popups: []` clears prior surfaces even in a delta with
no changed buffer rows. Opening, updating and closing a popup therefore never
leave an older overlay attached to a new source frame.

While any visible popup exists, native source pointer and precision-scroll
requests are disabled and rejected by the bridge. Hosts retire pending native
scroll/drag transforms and retain the original PTY input route until both the
live and last presented overlay are gone. Vim owns keyboard filters, selection,
`moved`, `close`, input and lifetime throughout.

This version accepts complete, unwrapped text, including default `wrap` when
no line actually wraps. Scrolled/clipped/wrapped content, titles, resize/drag,
filters, virtual text, conceal, unsupported decorations and excessive budgets
fall back explicitly. It does not add a rich Markdown/image document renderer,
inline inlay layout, or generic interactive popup widgets. Protocols 1–5
retain their previous popup fallback. `runtime/vim9/test_render_popups.py`
exercises real Vim popups, completion-plus-docs, styles, Unicode, lifecycle,
input ownership and fallback; each host adds renderer/controller coverage.

## Native visible whitespace

Common `listchars` settings retain GUI buffer painting, including explicit
two/three-glyph `tab`, `space`, `lead`, `trail`, `nbsp` and `eol` fields with
single-cell symbols. For example:

```vim
set list listchars=tab:»·,trail:·,nbsp:␣,eol:↲
```

Symbols keep Vim's GUI `SpecialKey`/`NonText` colors, source-byte positions and
selection/search composition. Tab display cells all refer to the same source
Tab byte; an EOL symbol has no source span. Saving retains the original text.
Window-local option changes invalidate native paint without requiring an edit.

Unsupported marker fields, default `^I` Tab geometry, actually visible
`extends`/`precedes` markers and wrapped EOL continuation rows retain explicit
compatibility reasons. The host does not rewrite the user's display options.

## Native CodeLens and diagnostic labels

SimpleCC's `:SimpleCCCodeLens` and enabled virtual diagnostics use Vim text
properties after the original source line. The bridge now paints those labels
with the same GUI font/RGB path as the buffer. Padding, property priority,
creation order, Unicode clusters, CursorLine, colorcolumn, listchars EOL,
split viewports and horizontal clipping are derived from Vim's public APIs.
Virtual runs have `line=0` and empty source-byte ranges; labels never enter the
buffer, undo history or saved file. Clicking an after-label retains Vim's EOL
cursor behavior. Precision scrolling keeps its existing source/window checks.

`Ctrl+Shift+P` exposes `>simple` actions to refresh CodeLens, run a nearby lens
and toggle SimpleCC inlay hints when those commands are available. Existing
SimpleCC commands and mappings retain ownership of command execution; the
renderer does not execute label text. `User SimpleCCDecorationsChanged`
refreshes additions, replacements and removals without waiting for a buffer
edit. Row deltas explicitly clear removed labels while retaining source text.

The supported subset is `col=0`, `text_align=after` (including the default)
and `text_wrap=truncate` (including the default). Normal/Insert/Replace views
may use it. Inline inlay hints, above/below/right alignment, requested wrapping,
Visual selections with virtual labels, control/bidirectional text, partially
clipped wide glyphs and truncation while `wrap` is enabled still reveal the
real Vim compatibility view. No option or hint is silently disabled. Inlay
hints are separate from CodeLens/diagnostic labels; `:SimpleCCInlayHints`
explicitly toggles them if the user wants to keep the supported EOL labels
on the native surface.

The bounds are 128 labels per line, 1,024 characters/4,096 UTF-8 bytes per
label, 32,768 virtual-label bytes per sampled window, and the existing total
property/frame budgets. Protocol 6 is unchanged because source-free runs
already carry this paint. `g:jterm_vim9_virtual_text_after` identifies the
new authenticated producer capability and is cleared before disconnect hooks;
the protocol number alone does not imply this feature.

`runtime/vim9/test_render_virtual_text.py` compares the paint with actual Vim
screen glyphs and highlight identities, checks source/save/click semantics,
split margins, horizontal scroll, property-only delta lifecycle and explicit
fallback recovery. Host painter tests cover the corresponding GUI surfaces;
these checks do not establish desktop latency or arbitrary virtual layouts.

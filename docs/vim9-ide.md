# Native Vim9 workbenches

Anvil, Forge, Ember and Frost share an editor bridge around a **real Vim 9
process**. Anvil and Forge host the editor in a dedicated GTK/VTE workbench;
Ember and Frost use their existing native GPU terminal grid in an editor tab.
The workbench supplies project navigation, buffer tabs, code actions, project
search, build controls, quickfix navigation and editor state.

Vim owns the text, modes, undo tree, registers, macros, splits, completion and
plugins. Native controls send typed actions; they do not type Ex commands into
an arbitrary shell or emulate Vim editing. The center editor is Vim's terminal
UI rendered by the frontend, rather than a replacement text widget.

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
Anvil and Forge open a separate workbench process, so closing the terminal
window that launched it does not terminate the editor. Ember and Frost create
an editor tab in their existing terminal application.

## Edit, navigate, search and build

Open files from the native file controls. The buffer strip reflects Vim's
listed buffers and marks modified buffers. Clicking a problem or search result
opens its real filename and line/column. Save, Save All, buffer changes and
diagnostic navigation retain Vim's ordinary errors and modified-buffer checks.
Vim commands such as `:vsplit`, `:tabnew`, `:undolist` and your existing mappings
continue to work in the editor.

Project search uses `rg` for literal text matches and populates quickfix results.
Install ripgrep for this action. Search text is passed as an argv argument
rather than shell code; ripgrep's normal ignore rules apply. The workbench
shows search progress, errors, the retained result count and omitted matches.
Build runs the command explicitly entered in the workbench asynchronously using
Vim's shell configuration and parses output with Vim's `errorformat`. An empty
build field uses Vim's `makeprg`. Build never saves buffers implicitly. For
example, use `cargo check --message-format=short`, `make`, or a project's test
command. Select the corresponding Vim `:compiler` or configure `errorformat`
when a tool's output format differs. The editor remains usable while the job
runs; its running state and exit code are shown, and Stop Build requests
cancellation. No project build is run automatically just by opening a folder.

Quickfix is the common problem/result surface: a later build or search replaces
the current list according to the runtime workflow. At most 256 buffers and
256 diagnostic entries are sent to native controls. Project search retains
at most 256 matches in quickfix and reports omissions. Build captures at most
10,000 output lines for quickfix parsing and shows an 80-line output tail in
the native workbench. Other Vim buffers and quickfix lists remain owned by
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

## Verification

`cargo test vim9::tests` exercises real loopback connections, authentication,
fragmented input, bounded queues/frames, editor-state synchronization and
disconnection. The runtime directory includes an integration test that starts
an actual PTY Vim process and drives its native channel and editing behavior.
The ignored `vim9::tests::real_vim9_round_trip` test additionally checks the
complete typed Rust controller against real Vim: launch, open a special
filename, edit, refuse an unsaved quit, save, parse a failed build and quit.
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

# Shell Vim entry and precise native scrolling

## Starting the editor

In a managed local interactive shell, `vim`, `vim +42 file`, `vim -d a b` and
multi-file invocations open that terminal frontend's native Vim editor window.
The originating shell waits; quitting Vim returns its exit status to that
shell. This is a separate native window/process in the current architecture,
not an in-place replacement of an arbitrary running terminal application.

The shell first performs its usual expansions and argument quoting. A private,
executable cache shim forwards those arguments one by one via
`--vim-shell /absolute/original/vim -- ...`. It does not parse shell text,
evaluate filenames, or modify shell rc files. The Vim startup receives the
original cwd, all filenames, `+line`, diff flags, custom vimrc options and
startup commands; the bridge is added as its own `-S` startup script.

Each native launch retains a real Vim process and a fresh authenticated bridge.
Vim continues to own edits, undo, registers, mappings, modes, plugins and file
writes. Titles, cwd messages, command-output strings and restored sessions
still cannot mint a native editor controller.

```toml
[vim9]
auto_gui = true
native_buffer = true
smooth_scroll = true
toolbar = false
buffer_tabs = false
status_bar = false
project_panel = false
tool_panel = false
```

`auto_gui = false` disables automatic integration for new shells. The single
command escape hatch is `JTERM_VIM9_AUTO_GUI=0 vim file`. An explicit executable
path such as `/usr/bin/vim` also bypasses the shim. `--ide` remains available.
Shell aliases or functions defined by the user retain their normal precedence.

The integration is child-local. Bash receives an exported `vim` function so a
login profile resetting PATH cannot erase it. The hosts' owned Bash-to-jsh
launcher reapplies the shim directory after rc processing. Other shells use
the inherited PATH; a user startup script that replaces it must preserve the
`JTERM_VIM9_SHIM_DIR` prefix to retain this integration. The first release skips
Flatpak namespace crossing. The PATH shim is not installed on a remote SSH host.

Piped/redirected stdin or stdout, `vim -`, Ex/silent/script-input mode, help or
version queries, explicit external GUI options (`-g`/`-y`), and Vim client/server
options execute the original Vim.
Missing display/frontend capability also uses the original executable.
The real Vim executable must already be installed; this feature does not
download or substitute an editor. The native bridge currently requires UTF-8
paths and arguments, as the existing native editor does.

## Wheel input and presentation

Native buffer wheel input uses a typed `Scroll { window, lines }` command.
The target is the split under the pointer; scrolling an inactive split does
not focus it. A late event referring to a closed window is rejected, rather
than retargeted to whatever window is now active. The command operates only
on ordinary editor windows in the current tab and respects Vim's busy modes.

`Vim9ScrollAccumulator` retains fractional line deltas per window. Hosts
convert touchpad pixels using the actual buffer line height, while a discrete
wheel notch normally corresponds to three lines. Reversing direction starts
the new gesture immediately. Invalid/nonfinite input is discarded and each
command is bounded to 120 lines.

After Vim acknowledges a same-buffer viewport move, `Vim9ScrollAnimation`
interpolates the old and new visible rows over a short 100 ms cubic easing tail.
Its progress uses elapsed time rather than a fixed frame count. Retargeting an
active animation starts at its current offset. Hosts clip each split separately
and retain previous edge rows so the transition does not expose a blank strip.
Editing, mouse actions, geometry/font changes, buffer changes or compatibility
fallback settle the transition before new input is interpreted.

Setting `smooth_scroll = false` keeps precise wheel accumulation and native
scroll commands while removing the easing tail. The GTK implementation also
respects the system's animation preference.

The transport wakes its socket worker when a command is enqueued instead of
waiting for the former 20 ms read timeout. Idle waiting uses descriptor polling,
not a faster busy-poll loop. Requests and paint frames retain their existing
size, identity, ordering, and queue bounds.

## Validation and current limits

`test_shell.py` exercises the shipped POSIX shim with real PTYs, exact unusual
filenames, multi-file/diff/line arguments, working directory, exit status,
batch/pipe/opt-out fallback and repeated shim PATH entries. `test_scroll.py`
uses a real Vim process to verify inactive-split scrolling, focus and edit
invariants, Insert-mode Unicode input/save, viewport bounds, invalid requests
and command-line refusal. Rust tests cover entry boundaries, fractional input,
animation continuity, bridge lifecycle and an actual Vim round trip. Frontend
window tests are required in addition to these shared tests.

The protocol currently publishes visible rows. Tiny sub-line input accumulates
until a real row move can be requested; presentation easing starts after that
move is acknowledged. It is not yet an overscanned pixel viewport that responds
to every sub-pixel input before Vim advances a row. Folding, wrapped lines and
other unsupported display states retain the existing compatibility behavior.
Physical high-refresh monitors, real touchpad hardware, Wayland compositors and
OS input methods require desktop validation. This release does not establish a
comparative performance result against VS Code or a universal SOTA claim.

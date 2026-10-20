# Investigating cwd updates and unexpected editor/full-screen modes

`tests/cwd_mode_isolation.rs` guards the boundary between shell metadata and
terminal full-screen control. It is not a reproduction of an IDE auto-launch
incident and does not change runtime behavior.

Run the protocol guards with:

```sh
cargo test --locked --test cwd_mode_isolation
```

The tests preserve ordinary OSC 7 / 1337 / title updates, including Unicode
and spaces, through every two-part read boundary. They also exercise OSC 133
shell lifecycle, remote session metadata, interleaved parser owners, repeated
directory updates after editor exit, and real subsequent DECSET/DECRST
full-screen transitions. A title containing a JSON `hello` event remains title
text. None of this traffic authenticates a native Vim9 bridge.

## Native investigation probe

`scripts/probe-cd-mode-gui.py` starts a separate Xvfb and one explicitly selected
terminal binary. It creates private HOME/XDG directories and configuration,
then drives actual XTest keyboard input using `xdotool`. It needs Python 3,
Xvfb, xdotool, xwininfo, ImageMagick `import`, `/bin/bash`, and `/usr/bin/vim`.
It uses software graphics and paces ordinary character input at 8 ms; only
the intentional Tab/Enter case uses zero key delay. No OCR tool is needed.

```sh
python3 scripts/probe-cd-mode-gui.py anvil \
  --binary /absolute/path/to/anvil \
  --shell bash --output /absolute/new/evidence-directory

python3 scripts/probe-cd-mode-gui.py ember \
  --binary /absolute/path/to/ember --shell jsh \
  --jsh-binary /absolute/path/to/jsh \
  --output /absolute/new/evidence-directory
```

The application name can be `anvil`, `forge`, `ember`, or `frost`. The output
directory must not exist. Shell readiness is checked through the launched
process's descendants and terminal raw-mode flags before typing; a startup
delay must not turn into a false command-input failure.

The probe covers directories named `vim`, `nvim codex`, and `空 格 雪`, five
navigation cycles including `cd -`, 20 Tab-completion/Enter cycles (including
closely batched keys), a rapid 20-cd loop, a Vim-like OSC title,
real Vim entry/exit, and two terminal tabs. Acceptance checks use command
sentinels, exact resulting directories, and shell PID continuity. Screenshots
show startup, the state after cd, actual Vim, and the final shell. `result.json`
also records the binary's SHA-256 and the final X window tree. Inspect the
screenshots when diagnosing mode changes: command execution alone cannot
certify the absence of a brief visual transition.

This probe does not exercise an authenticated SSH server, truly simultaneous
background-tab updates, user-specific shell hooks, IME composition, or native
IDE toolbar activation. Those need their own incident evidence. The Vim9
runtime's existing Python and Rust integration suites and frontend-native
tests cover the separate explicit IDE lifecycle.

## Interpretation

A passing probe bounds what was tested; it does not establish that a reported
intermittent problem is fixed. Before changing runtime mode policy, identify
whether the symptom is native IDE chrome, an ordinary alternate-screen TUI,
or only a retained title. Record the exact binary, backend, shell/hooks,
source tab/session, and the first triggering event. Keep metadata, explicit
user IDE actions, terminal DECSET/DECRST, and the authenticated Vim9 channel
distinct when following the production event path.

`--explicit-ide` captures an explicitly launched workbench instead of running
shell commands. This diagnostic mode reports `captured`, not an authentication
or editing-test success; inspect `explicit-ide.png` and use the native runtime
and frontend integration suites for those assertions.

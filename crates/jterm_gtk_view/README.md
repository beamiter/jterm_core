# Shared GTK terminal candidates

Forge and Anvil use this crate for the native text view and its input,
clipboard, viewport and frame handling. Each application supplies its
existing owned PTY transport and makes the **New shared terminal tab**
action available in the local candidate build. A shared tab uses `TERM=dumb`
and lasts for the current window. Candidate builds inject dependencies in a
private manifest and preserve the published application pins.

Run the real X11 shell journey from the `jterm_core` checkout. It uses
PyGObject AT-SPI, `xdotool`, `xclip`, `xvfb-run` and a D-Bus session.

```sh
python3 scripts/family-cargo.py --forge-shared-terminal forge build --release --bin forge
dbus-run-session -- xvfb-run -a /usr/bin/python3 crates/jterm_gtk_view/tests/native.py forge ../forge/target/release/forge
python3 scripts/family-cargo.py --anvil-shared-terminal anvil build --release --bin anvil
dbus-run-session -- xvfb-run -a /usr/bin/python3 crates/jterm_gtk_view/tests/native.py anvil ../anvil/target/release/anvil
```

The journey opens two live tabs through the application action, reads their
native accessible text, checks the kernel PTY resize, verifies that clipboard
text waits for an explicit Enter, and verifies hidden output and shell exit.
Scrollback uses the terminal's retained, styled physical rows. Use the scroll
wheel or scrollbar, Shift+Page Up and Shift+Page Down to move by page, or
Shift+Home and Shift+End to reach the oldest page or live screen. Each tab
keeps its reading position when output arrives or its viewport resizes while
the anchored row remains in history.
Typing resumes following the live screen. The journey verifies keyboard and
wheel navigation, resize while reading, and returning to a tab that emitted
output in the background. History reflow and full TUI support remain separate
gates. Released Block and Chat builds use VTE for visible completed cards.

The standalone shared terminal paints explicit SGR 58 underline colors in
live cells and retained history. Add `--underline-colors` to each X11 command
to check the rendered pixels with ImageMagick `import`. The fixture sends
RGB and indexed colors through the real shell, compares the underline with
unchanged glyph pixels, and reads back the indexed color from scrollback.

Ctrl+click opens an accepted OSC 8 HTTPS or HTTP link from the live screen
or history through the application's existing opener. The gesture validates
the same rendered text and target at press and release. Add `--osc8-links`
to the X11 commands to locate link text with ImageMagick `import` and verify
both clicks with an isolated local GIO handler.

### Shared local session owner

Set `JTERM_SHARED_RUNTIME=1` before launching a Linux candidate with a direct
local `/bin/sh` or `/usr/bin/sh` login or interactive argv. **New shared
terminal tab** opens the configured shell and working directory through
`jterm_runtime`. The runtime owns the PTY, terminal state, bounded input
queue, replies, resize, and exit. GTK renders its frames and history and
keeps draining output when the tab is hidden. Other launch profiles use
their existing transport, including Flatpak host shells.

Add `--shared-runtime` to the X11 or Wayland fixture commands to verify this
ownership route. X11 checks that exactly one PTY device-status reply arrives
before the next typed byte, then exercises resize, clipboard, history, input
effects, and a hidden tab. Wayland checks native IME and clipboard on the
same runtime route.

### Wayland input method

The shared terminal displays GTK input method preedit at the painted cursor
and exposes the composition through accessibility. Only committed text enters
the PTY. Leaving the tab or scrolling into history cancels composition.

The native fixture needs Sway, `wtype`, `wl-copy`, `wl-paste`, PyGObject
AT-SPI and the input-method-v2 test client from the sibling Frost checkout.
It starts its own compositor with XWayland disabled and records compositor,
application and PTY evidence in the output directory.

```sh
test_tools=$(mktemp -d)
ime_protocol=../frost/scripts/protocols/input-method-unstable-v2.xml
wayland-scanner client-header "$ime_protocol" "$test_tools/input-method-unstable-v2-client-protocol.h"
wayland-scanner private-code "$ime_protocol" "$test_tools/input-method-unstable-v2-protocol.c"
cc -I"$test_tools" ../frost/scripts/native-ime-client.c \
  "$test_tools/input-method-unstable-v2-protocol.c" -lwayland-client -o "$test_tools/native-ime-client"
pointer_protocol=../frost/scripts/protocols/wlr-virtual-pointer-unstable-v1.xml
wayland-scanner client-header "$pointer_protocol" "$test_tools/wlr-virtual-pointer-unstable-v1-client-protocol.h"
wayland-scanner private-code "$pointer_protocol" "$test_tools/wlr-virtual-pointer-unstable-v1-protocol.c"
cc -I"$test_tools" ../frost/scripts/native-pointer.c \
  "$test_tools/wlr-virtual-pointer-unstable-v1-protocol.c" -lwayland-client -o "$test_tools/native-pointer"
dbus-run-session -- /usr/bin/python3 -B crates/jterm_gtk_view/tests/native_wayland.py \
  forge ../forge/target/release/forge --ime-client "$test_tools/native-ime-client" \
  --pointer-client "$test_tools/native-pointer" --output "$test_tools/forge-evidence"
dbus-run-session -- /usr/bin/python3 -B crates/jterm_gtk_view/tests/native_wayland.py \
  anvil ../anvil/target/release/anvil --ime-client "$test_tools/native-ime-client" \
  --pointer-client "$test_tools/native-pointer" --output "$test_tools/anvil-evidence"
```

The fixture checks terminal and preedit text through AT-SPI, nonzero
text-input-v3 cursor geometry, exact committed UTF-8 captured by the live
PTY, and a Wayland clipboard paste with a trailing newline. The pointer
option verifies native double-click selection and external clipboard copy.

### Retained output

Background cards in both applications have no command terminal. Unified's
retained **Output Snapshot** dialog uses the shared `DocumentView` in
candidate builds. It accepts the same already-bounded output from each app,
exposes GTK's read-only text selection and clipboard, and searches within
that snapshot without opening or attaching to a shell.

Verify the mapped document's UTF-8 selection, clipboard, and find/wrap:

```sh
dbus-run-session -- xvfb-run -a cargo test --offline --locked --release \
  --manifest-path crates/jterm_gtk_view/Cargo.toml \
  document::tests::retained_output_is_read_only_searchable_and_selectable -- --ignored --exact
```

## Completed command rows

For a candidate build, set `JTERM_SHARED_FINISHED_COMMANDS=1` when launching
Forge or Anvil. Newly captured Block and Chat commands use the shared command
view when the shell reports an exact, short, single-line command supported
by the view. It retains syntax colors and participates in per-card and
cross-card search, native selection, and bounded mixed-surface copying.
The URL, path, multiline, and restored command rows continue to use their
existing renderers.

From `jterm_core`, validate the mapped Block journey with the candidate
dependency and Xvfb:

```sh
JTERM_SHARED_FINISHED_COMMANDS=1 dbus-run-session -- xvfb-run -a \
  python3 scripts/family-cargo.py --forge-shared-terminal forge test --release --lib \
  block_view::cross_selection::tests::shared_finished_command_preserves_search_and_cross_card_copy -- --ignored --exact
JTERM_SHARED_FINISHED_COMMANDS=1 dbus-run-session -- xvfb-run -a \
  python3 scripts/family-cargo.py --anvil-shared-terminal anvil test --release --bin anvil \
  block_view::cross_selection::tests::shared_finished_command_preserves_search_and_cross_card_copy -- --ignored --exact
```


## Completed output rows

Set both `JTERM_SHARED_FINISHED_COMMANDS=1` and
`JTERM_SHARED_FINISHED_OUTPUT=1` on a candidate launch. Eligible foreground
Block and Chat records present their output in the shared GTK document
view. The finalized capture must fit within twelve logical lines, two KiB
of visible text, eight KiB of decorated data, and the configured card
viewport. The shared archive projection preserves supported ANSI palette,
indexed and truecolor styles plus bold, italic, underline and strike as
byte ranges on the exact visible text. Links, paths, non-ASCII clusters,
images, partial captures, cursor controls and unsupported styles such as
blink continue to use the existing archive renderer. The retained payload
supplies copy, export and filtering.

Run the same Xvfb commands above with both environment variables set and
the test filter
`block_view::cross_selection::tests::shared_finished_output_preserves_search_filter_and_cross_card_copy`.
For Chat, use
`block_view::cross_selection::tests::shared_finished_chat_preserves_capsule_search_filter_and_copy`
to check that the content-sized command capsule and result keep their layout.

The fixtures send real OSC 133 completion data through the application
PTY readers and check six mapped styled lines, fold-aware find, native
clipboard, style-preserving filtering, mixed copy, and URL and unsupported
ANSI fallbacks.

## Running command output

Set `JTERM_SHARED_LIVE_OUTPUT=1` on a candidate launch. The active Block and
Chat card can allocate its shared GTK row to one complete, unwrapped ASCII
output line. The current visible row must match the bounded capture and have
plain attributes in the legacy terminal export. The GTK snapshot clips the
legacy paint for those cells, preserving transparent card backgrounds. The
presentation has no input, parser, replies, or PTY handle. The existing
session owns shell input, find and selection, and presents links, styled
output, scrollback and alternate-screen applications.

Run the native Block and Chat journey with a real long-lived shell in both
candidate configurations. It checks a live projected line, active find,
alternate-screen fallback, completion, and the next command in the same PTY.

```sh
JTERM_SHARED_LIVE_OUTPUT=1 dbus-run-session -- xvfb-run -a \
  python3 scripts/family-cargo.py --forge-shared-terminal forge test --release --lib \
  block_view::cross_selection::tests::shared_live_block_and_chat_keep_the_owned_shell -- --ignored --exact
JTERM_SHARED_LIVE_OUTPUT=1 dbus-run-session -- xvfb-run -a \
  python3 scripts/family-cargo.py --anvil-shared-terminal anvil test --release --bin anvil \
  block_view::cross_selection::tests::shared_live_block_and_chat_keep_the_owned_shell -- --ignored --exact
```

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

Linux Forge and Anvil builds with the `shared-terminal` feature use
`jterm_runtime` by default for ordinary tabs and splits configured with
`terminal_mode = "vte"` and a direct local sh or Bash argv. The runtime
owns the PTY, terminal state, bounded input queue, replies, resize and exit.
GTK renders its frames and history and keeps draining output when the tab is
hidden. `JTERM_SHARED_RUNTIME=0` selects the existing compatibility route.
Host-bridged shells and custom command launches use the compatibility transport.

With `JTERM_SHARED_RUNTIME=1` and `terminal_mode = "vte"`, ordinary local jsh
panes also use the shared runtime. The application passes its pane's saved
session ID to jsh as a separate argv value. The Bash startup wrapper keeps
the same process and terminal while it changes its executable to jsh.

Use `--ordinary-panes` for the X11 fixture and `--ordinary-pane` for the
Wayland fixture to check the default. For the standalone tab action, use
`--shared-runtime` to verify ordered PTY replies and interactive input.
The native checks also cover resizing, clipboard, history and hidden output.

Run the jsh fixture with an installed jsh executable. It opens a real split,
closes the window, and verifies the saved jsh session ID, working directory
and exported variable after restarting the application.

```sh
dbus-run-session -- xvfb-run -a /usr/bin/python3 -B crates/jterm_gtk_view/tests/native_jsh.py \
  forge ../forge/target/release/forge --jsh "$(command -v jsh)"
dbus-run-session -- xvfb-run -a /usr/bin/python3 -B crates/jterm_gtk_view/tests/native_jsh.py \
  anvil ../anvil/target/release/anvil --jsh "$(command -v jsh)"
```

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

Forge and Anvil builds with `shared-terminal` use a shared command view for
eligible shell-reported Block and Chat commands. The view preserves syntax
colors, native selection, per-card search and bounded cross-card copying.
Set `JTERM_SHARED_FINISHED_COMMANDS=0` to use VTE for the entire card.

## Completed output rows

The shared GTK document view displays eligible finalized output. Its capture
can contain up to twelve logical lines, two KiB of visible text, and eight KiB
of styled data. It also fits within the configured card viewport. The view
preserves supported ANSI styles, grapheme clusters, OSC 8 links, and plain
HTTP and HTTPS links. Images, partial captures, cursor controls and
unsupported styles use the compatibility renderer. The retained payload
supplies copy, export and filtering.

Set `JTERM_SHARED_FINISHED_OUTPUT=0` to use VTE for completed output.
Run the native GTK card fixture from either application checkout against
its locked core. It tests the default with both overrides unset.

```sh
core=$(python3 scripts/shared-core-path.py)
bash "$core/crates/jterm_gtk_view/tests/test-native-cards.sh" forge /tmp/forge-card-evidence
```

Use `anvil` as the first argument in an Anvil checkout. The fixtures feed OSC
133 completion data through the PTY readers and check mapped styled lines,
fold-aware search, filtering, mixed clipboard selection and compatibility
rendering for unsupported ANSI.

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

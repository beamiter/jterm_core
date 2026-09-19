# screen_replay fixtures

Real PTY byte streams used by `src/screen_replay/tests.rs`, and the libvte side
of its differential test.

All captures were recorded on 2026-09-19 with `ptycap.py` (a PTY harness that
answers the queries a VTE-class terminal answers: CPR, DA1/DA2, XTVERSION,
OSC 10/11, kitty `?u`, DECRQM) under `TERM=xterm-256color`,
`COLORTERM=truecolor`, `VTE_VERSION=7600`. No prompt was ever submitted to an
agent; typed text never ended with a carriage return. Geometry below is
rows x cols; offsets are byte offsets into the file at which the winsize
changed.

| file | program | start | resizes | notes |
|---|---|---|---|---|
| `codex-16rows.bin` | codex 0.155.0 | 16x120 | none | startup inline; history insertion via `ESC[1;16r` + `ESC M` and `ESC[1;7r` + `\r\n`; contains the "Tip: Try the Desktop app on Linux" and "usage limit" history lines. Typed `explain this repo` at 6.0 s (offset 16981), not submitted. |
| `codex-40-resize.bin` | codex | 40x120 | 41x120 @ 17884, 41x122 @ 46533 | codex answers each resize with `CSI 2J CSI 3J` and a full replay. Typed `hello there` at 4.0 s, two Ctrl+C at the end. |
| `claude-classic.bin` | claude 2.1.278, `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` | 40x120 | 41x118 @ 1833 | inline Ink rendering. Typed `hello there`, two Ctrl+C. |
| `claude-fullscreen.bin` | claude 2.1.278 | 40x120 | 41x120 @ 1753 | runs in the alternate screen (`?1049h`); nothing reaches the primary screen. |
| `kimi-40-resize.bin` | kimi | 40x120 | 41x120 @ 1774 | trust dialog only. |

`vte_export.py` feeds a stream (optionally with the resizes above) to a real
`Vte.Terminal` (GTK4, Vte 3.91, libvte 0.76) with unlimited scrollback and
prints `write_contents_sync`. Run the differential test through a headless
display, e.g.

    headless-gtk.sh cargo test --lib \
        screen_replay::tests::vte_differential_matches_real_vte -- --ignored --exact

use super::*;
use std::fmt::Write as _;

const CODEX_16ROWS: &[u8] = include_bytes!("../../tests/fixtures/screen_replay/codex-16rows.bin");
const CODEX_40_RESIZE: &[u8] =
    include_bytes!("../../tests/fixtures/screen_replay/codex-40-resize.bin");
const CLAUDE_CLASSIC: &[u8] =
    include_bytes!("../../tests/fixtures/screen_replay/claude-classic.bin");
const CLAUDE_FULLSCREEN: &[u8] =
    include_bytes!("../../tests/fixtures/screen_replay/claude-fullscreen.bin");
const KIMI_40_RESIZE: &[u8] =
    include_bytes!("../../tests/fixtures/screen_replay/kimi-40-resize.bin");

fn replay(cols: usize, rows: usize, bytes: &[u8]) -> Replay {
    let mut replay = ScreenReplay::new(cols, rows);
    replay.feed(bytes);
    replay.finish()
}

fn plain(cols: usize, rows: usize, bytes: &[u8]) -> String {
    replay(cols, rows, bytes).to_plain()
}

/// Asserts that every needle occurs in `haystack`, in order.
fn assert_in_order(haystack: &str, needles: &[&str]) {
    let mut from = 0;
    for needle in needles {
        match haystack[from..].find(needle) {
            Some(at) => from += at + needle.len(),
            None => panic!("{needle:?} missing (or out of order) in:\n{haystack}"),
        }
    }
}

// ---- synthetic streams ---------------------------------------------------

fn cup(out: &mut Vec<u8>, row: usize, col: usize) {
    let _ = write!(StrBuf(out), "\x1b[{};{}H", row + 1, col + 1);
}

/// `write!` into a byte vector.
struct StrBuf<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for StrBuf<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// A codex-style inline session, following codex's `insert_history.rs`
/// (standard mode): a `viewport`-row live area that starts at the top of a
/// `rows`-row screen and is pushed down as history is inserted above it;
/// once it reaches the bottom, each batch is inserted with
/// `SetScrollRegion(1..area.top)`, `MoveTo(0, top-1)` and `\r\n` + line per
/// history line, which a real terminal turns into scrollback. After every
/// batch the viewport is redrawn; at exit it is cleared and the resume hint
/// printed. The viewport starts on row 1, under the line the command was
/// typed on (codex places it at the cursor; with the viewport at row 0 its
/// first `SetScrollRegion(1..1)` is invalid and ignored, in VTE too).
fn synth_codex(lines: usize, rows: usize, viewport: usize) -> Vec<u8> {
    let mut out = b"$ codex\r\n".to_vec();
    let mut area_top = 1usize;
    let draw_viewport = |out: &mut Vec<u8>, top: usize, tick: usize| {
        for r in 0..viewport {
            cup(out, top + r, 0);
            out.extend_from_slice(b"\x1b[K");
            let text = match r {
                0 => format!("\x1b[2m• Working ({tick}s • esc to interrupt)\x1b[0m"),
                2 => "\x1b[48;2;57;57;71m› Ask Codex to do anything\x1b[K\x1b[0m".to_string(),
                4 => "  ? for shortcuts".to_string(),
                _ => String::new(),
            };
            out.extend_from_slice(text.as_bytes());
        }
        cup(out, top + 2, 2);
    };
    draw_viewport(&mut out, area_top, 0);
    let mut next = 0;
    let mut tick = 0;
    while next < lines {
        // Batches of 1..=3 lines, like streamed answer chunks.
        let batch = (1 + tick % 3).min(lines - next);
        let cursor_top = if area_top + viewport < rows {
            let scroll = batch.min(rows - (area_top + viewport));
            let _ = write!(StrBuf(&mut out), "\x1b[{};{}r", area_top + 1, rows);
            cup(&mut out, area_top, 0);
            for _ in 0..scroll {
                out.extend_from_slice(b"\x1bM");
            }
            out.extend_from_slice(b"\x1b[r");
            let cursor_top = area_top.saturating_sub(1);
            area_top += scroll;
            cursor_top
        } else {
            area_top.saturating_sub(1)
        };
        let _ = write!(StrBuf(&mut out), "\x1b[1;{}r", area_top);
        cup(&mut out, cursor_top, 0);
        for _ in 0..batch {
            let _ = write!(
                StrBuf(&mut out),
                "\r\n\x1b[1mhistory\x1b[22m line {next:03}"
            );
            next += 1;
        }
        out.extend_from_slice(b"\x1b[r");
        tick += 1;
        draw_viewport(&mut out, area_top, tick);
    }
    cup(&mut out, area_top, 0);
    out.extend_from_slice(b"\x1b[J");
    out.extend_from_slice(b"To continue this session, run codex resume 0199\r\n");
    out
}

/// Ink's `eraseLines(n)`: clear the line, go up, … clear the first line and
/// return to column 1.
fn erase_lines(out: &mut Vec<u8>, n: usize) {
    for i in 0..n {
        out.extend_from_slice(b"\x1b[2K");
        if i + 1 < n {
            out.extend_from_slice(b"\x1b[1A");
        }
    }
    out.extend_from_slice(b"\x1b[G");
}

/// An Ink-style session (claude classic, kimi): `lines` static lines printed
/// above a three-row composer that is erased and redrawn for each one, then
/// a final static "FINAL ANSWER" line and an unmount that erases the
/// composer.
fn synth_ink(lines: usize) -> Vec<u8> {
    const COMPOSER: &str =
        "\x1b[2m╭────────╮\x1b[0m\n│ > \x1b[7m \x1b[0m     │\n╰────────╯ ? for shortcuts\n";
    let composer_rows = COMPOSER.matches('\n').count() + 1;
    let mut out = Vec::new();
    out.extend_from_slice(COMPOSER.as_bytes());
    for i in 0..lines {
        erase_lines(&mut out, composer_rows);
        let _ = writeln!(
            StrBuf(&mut out),
            "\x1b[38;5;{}m●\x1b[39m static line {i:05}",
            i % 256
        );
        out.extend_from_slice(COMPOSER.as_bytes());
    }
    erase_lines(&mut out, composer_rows);
    out.extend_from_slice(b"FINAL ANSWER\n");
    out
}

/// A `top`/`watch` loop: home, clear, draw a frame — forever.
fn synth_clear_loop(frames: usize, style_top: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for f in 0..frames {
        if style_top {
            // top: home, then every row cleared to EOL, then ED 0 below.
            out.extend_from_slice(b"\x1b[H");
            for r in 0..8 {
                let _ = write!(StrBuf(&mut out), "frame {f:03} row {r}\x1b[K\r\n");
            }
            out.extend_from_slice(b"\x1b[J");
        } else {
            out.extend_from_slice(b"\x1b[H\x1b[2J");
            for r in 0..8 {
                let _ = write!(StrBuf(&mut out), "frame {f:03} row {r}\r\n");
            }
        }
    }
    out
}

// ---- real captures -----------------------------------------------------

#[test]
fn codex_16_rows_keeps_history_inserted_above_its_viewport() {
    let text = plain(120, 16, CODEX_16ROWS);
    assert_in_order(
        &text,
        &[
            "OpenAI Codex",
            "Tip: Try the Desktop app on Linux",
            "usage limit",
        ],
    );
    let ansi = replay(120, 16, CODEX_16ROWS).to_ansi();
    assert!(ansi.contains("Tip:"), "the card keeps the tip too");
}

#[test]
fn codex_resize_replay_yields_one_transcript() {
    // codex answers each resize with `CSI 2J CSI 3J` and a full replay; at the
    // last size the child saw (41x122) that is exactly one transcript.
    let text = plain(122, 41, CODEX_40_RESIZE);
    assert_eq!(text.matches("OpenAI Codex").count(), 1, "{text}");
}

#[test]
fn claude_fullscreen_leaves_nothing_on_the_normal_screen() {
    // claude runs in the alternate screen; the primary screen stays empty
    // (apps never capture alt-screen bytes, but the replay must not leak
    // them either).
    let out = replay(120, 41, CLAUDE_FULLSCREEN);
    assert_eq!(out.row_count(), 0, "{:?}", out.to_plain());
}

// ---- synthetic TUIs ------------------------------------------------------

#[test]
fn synthetic_codex_keeps_all_150_history_lines_in_order() {
    let stream = synth_codex(150, 40, 6);
    for bytes in [stream.clone()] {
        let out = replay(120, 40, &bytes);
        let text = out.to_plain();
        let needles: Vec<String> = (0..150).map(|i| format!("history line {i:03}")).collect();
        let refs: Vec<&str> = needles.iter().map(String::as_str).collect();
        assert_in_order(&text, &refs);
        for needle in &needles {
            assert_eq!(text.matches(needle.as_str()).count(), 1, "{needle}");
        }
        assert!(!text.contains("Working"), "stale viewport rows: {text}");
        assert!(text.ends_with("To continue this session, run codex resume 0199"));
        assert!(!out.head_dropped);
    }
}

#[test]
fn ink_session_keeps_its_tail_and_evicts_its_head_under_a_small_budget() {
    let stream = synth_ink(12_000);
    let mut replay = ScreenReplay::with_budget(120, 40, 64 * 1024);
    for chunk in stream.chunks(4093) {
        replay.feed(chunk);
    }
    let out = replay.finish();
    assert!(out.head_dropped);
    let text = out.to_plain();
    assert!(
        text.ends_with("FINAL ANSWER"),
        "{}",
        &text[text.len() - 200..]
    );
    assert!(text.contains("static line 11999"));
    assert!(!text.contains("static line 00000"));
    assert!(!text.contains("for shortcuts"), "the composer was erased");
    // Every row kept is a whole static line; eviction removes whole rows.
    let first = text.lines().next().unwrap();
    assert!(first.starts_with("● static line "), "{first:?}");
    // The budget bounds what is kept (history rows are ≤ 19 cells here).
    assert!(out.row_count() * 19 <= 64 * 1024, "{}", out.row_count());
}

#[test]
fn ink_session_fits_the_default_budget_without_dropping() {
    let out = replay(120, 40, &synth_ink(2_000));
    assert!(!out.head_dropped);
    let text = out.to_plain();
    assert!(text.starts_with("● static line 00000"));
    assert!(text.ends_with("FINAL ANSWER"));
    assert_eq!(text.lines().count(), 2_001);
}

#[test]
fn clear_loops_collapse_to_the_final_frame() {
    for style_top in [false, true] {
        let out = replay(80, 24, &synth_clear_loop(200, style_top));
        let text = out.to_plain();
        let expected: Vec<String> = (0..8).map(|r| format!("frame 199 row {r}")).collect();
        assert_eq!(text, expected.join("\n"), "style_top={style_top}");
        assert_eq!(out.row_count(), 8);
    }
}

// ---- unit behaviour ------------------------------------------------------

#[test]
fn wide_characters_take_two_cells_before_a_cha() {
    // 中文ab ends at column 6; CHA 10 lands on column 9.
    assert_eq!(plain(20, 5, "中文ab\x1b[10GX".as_bytes()), "中文ab   X");
    // A wide character that does not fit wraps whole.
    assert_eq!(plain(5, 5, "abcd中".as_bytes()), "abcd中");
    assert_eq!(replay(5, 5, "abcd中".as_bytes()).row_count(), 2);
    // Combining marks join the previous cell instead of taking a column.
    assert_eq!(plain(20, 5, "e\u{301}x\x1b[4GY".as_bytes()), "e\u{301}x Y");
}

#[test]
fn autowrap_and_pending_wrap() {
    // Exactly full: the cursor parks, CR LF does not add a blank row.
    let out = replay(10, 5, b"0123456789\r\nnext");
    assert_eq!(out.to_plain(), "0123456789\nnext");
    assert_eq!(out.row_count(), 2);
    // One more character wraps, and the soft wrap joins in the text.
    let out = replay(10, 5, b"0123456789X\r\nnext");
    assert_eq!(out.to_plain(), "0123456789X\nnext");
    assert_eq!(out.row_count(), 3);
    // A CUB from the pending position moves from the last column.
    assert_eq!(plain(10, 5, b"0123456789\x1b[DZ"), "01234567Z9");
    // Without DECAWM the last column is overwritten.
    assert_eq!(plain(10, 5, b"\x1b[?7l0123456789XYZ"), "012345678Z");
    // Long output scrolls into history instead of being lost.
    let lines: String = (0..30).map(|i| format!("line {i}\r\n")).collect();
    let text = plain(20, 5, lines.as_bytes());
    assert_eq!(text.lines().count(), 30);
}

#[test]
fn sgr_carries_across_rows_and_diffs_minimally() {
    let out = replay(20, 5, b"\x1b[31mred\r\nstill red\x1b[0m plain");
    assert_eq!(out.to_ansi(), "\x1b[31mred\r\nstill red\x1b[0m plain");
    // 256-colour and truecolour, `;` and `:` forms.
    let out = replay(
        20,
        5,
        b"\x1b[38;5;208ma\x1b[38:2::1:2:3mb\x1b[48;2;4;5;6mc\x1b[m",
    );
    assert_eq!(
        out.to_ansi(),
        "\x1b[38;5;208ma\x1b[38;2;1;2;3mb\x1b[48;2;4;5;6mc\x1b[0m"
    );
    // A coloured background is reset before the row break, not bled on.
    let out = replay(20, 5, b"\x1b[44mblue\r\nx\x1b[m");
    assert_eq!(out.to_ansi(), "\x1b[44mblue\x1b[0m\r\n\x1b[44mx\x1b[0m");
    // Erasing with a background paints the erased cells.
    let out = replay(6, 2, b"\x1b[41m\x1b[K\x1b[m");
    assert_eq!(out.to_ansi(), "\x1b[41m      \x1b[0m");
    assert_eq!(out.to_plain(), "");
}

#[test]
fn osc8_hyperlinks_survive_per_cell_in_to_ansi() {
    let out = replay(
        40,
        5,
        b"see \x1b]8;id=a;https://example.test/x\x1b\\docs\x1b]8;;\x1b\\ and \x1b]8;;http://b.test\x07b\r\nc\x1b]8;;\x07",
    );
    assert_eq!(
        out.to_ansi(),
        "see \x1b]8;id=a;https://example.test/x\x1b\\docs\x1b]8;;\x1b\\ and \
         \x1b]8;;http://b.test\x1b\\b\x1b]8;;\x1b\\\r\n\x1b]8;;http://b.test\x1b\\c\x1b]8;;\x1b\\"
    );
    assert_eq!(out.to_plain(), "see docs and b\nc");
}

#[test]
fn unknown_and_string_sequences_print_nothing() {
    let bytes = b"a\x1b]0;title\x07b\x1bP1$r0m\x1b\\c\x1b_apc\x1b\\d\x1b[?1049;2004h\x1b[>4;1m\x1b[?1l\x1b=\x1b[5 qe\x1b[38;5m";
    let out = replay(40, 5, bytes);
    // `?1049h` switched to the alternate screen: the rest landed there.
    assert_eq!(out.to_plain(), "abcd");
    let out = replay(
        40,
        5,
        b"a\x1b]0;title\x07b\x1bP1$r0m\x1b\\c\x1b_apc\x1b\\d\x1b[>4;1me\x1b[?25l\x1b[12$pf",
    );
    assert_eq!(out.to_plain(), "abcdef");
}

#[test]
fn invalid_utf8_and_split_feeds() {
    assert_eq!(plain(20, 5, b"a\xffb\xe4\xb8c"), "a\u{fffd}b\u{fffd}c");
    let bytes = synth_codex(40, 12, 4);
    let whole = plain(120, 12, &bytes);
    let mut replay = ScreenReplay::new(120, 12);
    for byte in &bytes {
        replay.feed(std::slice::from_ref(byte));
    }
    assert_eq!(replay.finish().to_plain(), whole);
    let mut replay = ScreenReplay::new(20, 5);
    for chunk in "中\x1b[3".as_bytes().chunks(1) {
        replay.feed(chunk);
    }
    replay.feed(b"1mx");
    assert_eq!(replay.finish().to_ansi(), "中\x1b[31mx\x1b[0m");
}

#[test]
fn scroll_regions_reverse_index_and_line_editing() {
    // RI at the top margin scrolls the region down, leaving rows outside it.
    let out = plain(10, 4, b"a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[2;1H\x1bMX");
    assert_eq!(out, "a\nX\nb\nd");
    // A region that does not start at the top drops what scrolls out.
    let out = plain(10, 4, b"a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[3;1H\nY");
    assert_eq!(out, "a\nc\nY\nd");
    // IL/DL work inside the margins.
    assert_eq!(plain(10, 4, b"a\r\nb\r\nc\x1b[2;1H\x1b[L"), "a\n\nb\nc");
    // DL on row 0 of a full-screen region sends the row to history, as VTE does.
    assert_eq!(plain(10, 4, b"a\r\nb\r\nc\x1b[1;1H\x1b[M"), "a\nb\nc");
    assert_eq!(plain(10, 4, b"a\r\nb\r\nc\x1b[2;1H\x1b[M"), "a\nc");
    // DECSTBM with bottom <= top is ignored (the cursor is not homed).
    assert_eq!(plain(10, 4, b"ab\x1b[3;3rc"), "abc");
    // ICH/DCH/ECH and REP.
    assert_eq!(plain(10, 2, b"abcdef\x1b[1;2H\x1b[2@"), "a  bcdef");
    assert_eq!(plain(10, 2, b"abcdef\x1b[1;2H\x1b[2P"), "adef");
    assert_eq!(plain(10, 2, b"abcdef\x1b[1;2H\x1b[2X"), "a  def");
    assert_eq!(plain(10, 2, b"x\x1b[3b"), "xxxx");
    // DECSC/DECRC and CSI s/u.
    assert_eq!(plain(10, 3, b"a\x1b7\r\nb\x1b8c"), "ac\nb");
    assert_eq!(plain(10, 3, b"a\x1b[s\r\nb\x1b[uc"), "ac\nb");
    // EL 1 / EL 2 and ED 1.
    assert_eq!(plain(10, 3, b"abcdef\x1b[3D\x1b[1K"), "    ef");
    assert_eq!(plain(10, 3, b"abc\r\ndef\x1b[2K"), "abc");
    assert_eq!(plain(10, 3, b"abc\r\ndefg\x1b[2D\x1b[1J"), "\n   g");
}

#[test]
fn ed2_blanks_the_screen_and_ed3_drops_history() {
    let lines: String = (0..10).map(|i| format!("old {i}\r\n")).collect();
    // ED 2 keeps the history above the screen but blanks the screen.
    let text = plain(20, 4, format!("{lines}\x1b[2J\x1b[Hnew").as_bytes());
    assert!(text.starts_with("old 0\n"), "{text}");
    assert!(text.ends_with("old 6\nnew"), "{text}");
    // ED 3 then drops the history too.
    let text = plain(20, 4, format!("{lines}\x1b[2J\x1b[3J\x1b[Hnew").as_bytes());
    assert_eq!(text, "new");
}

#[test]
fn leading_untouched_rows_are_trimmed() {
    // Output that starts at an absolute row does not leave blank rows above.
    assert_eq!(plain(20, 10, b"\x1b[5;1Hfive\r\nsix"), "five\nsix");
    // Line feeds the program printed itself are kept.
    assert_eq!(plain(20, 10, b"\r\n\r\nthree"), "\n\nthree");
    assert_eq!(replay(20, 10, b"\x1b[H\x1b[2J").row_count(), 0);
}

#[test]
fn dec_special_graphics_and_tabs() {
    assert_eq!(plain(20, 2, b"\x1b(0lqk\x1b(Bx"), "┌─┐x");
    assert_eq!(plain(20, 2, b"\x1b)0\x0eq\x0fq"), "─q");
    // SO alone selects G1, which is ASCII until designated.
    assert_eq!(plain(20, 2, b"\x0eq\x0fq"), "qq");
    assert_eq!(plain(20, 2, b"a\tb"), "a\tb");
    // A tab on a default stop goes to the card as a tab, so a copy from the
    // card keeps it; one that ends elsewhere (cleared stops) stays spaces.
    assert_eq!(replay(20, 2, b"a\tb").to_ansi(), "a\tb");
    assert_eq!(
        replay(20, 2, b"\x1b[31ma\tb\x1b[m\tc").to_ansi(),
        "\x1b[31ma\x1b[0m\t\x1b[31mb\x1b[0m\tc"
    );
    assert_eq!(
        replay(20, 2, b"\x1b[3g\x1b[1;4H\x1bH\ra\tb").to_ansi(),
        "a  b"
    );
}

#[test]
fn to_ansi_keeps_soft_wraps_so_the_card_can_reflow() {
    // A full soft-wrapped row is continued, not hard-broken: the target
    // autowraps at the same width and joins the line when wider.
    let out = replay(10, 5, b"0123456789abc\r\nnext");
    assert_eq!(out.to_ansi(), "0123456789abc\r\nnext");
    // Feeding it back reproduces the same rows and logical lines.
    let again = replay(10, 5, out.to_ansi().as_bytes());
    assert_eq!(again.to_plain(), out.to_plain());
    assert_eq!(again.row_count(), out.row_count());
    // A wide character that did not fit leaves the last column free; the
    // target wraps it the same way, so the line is continued too.
    let out = replay(5, 5, "abcd中".as_bytes());
    assert_eq!(out.to_ansi(), "abcd中");
    // A short soft-wrapped row followed by a narrow one is a hard break.
    let out = replay(5, 5, "abcd中\x1b[2;1Hx".as_bytes());
    assert_eq!(out.to_ansi(), "abcd\r\nx");
    // A tab at the start of a continued row would be a no-op at the pending
    // wrap position, so it stays spaces there.
    let out = replay(16, 5, b"0123456789abcdefX\r\x1b[K\tx");
    assert_eq!(out.to_ansi(), "0123456789abcdef        x");
    let again = replay(16, 5, out.to_ansi().as_bytes());
    assert_eq!(again.to_plain(), "0123456789abcdef        x");
    assert_eq!(again.row_count(), 2);
}

#[test]
fn blank_history_rows_are_charged_and_evicted() {
    // Mass scrolling makes history rows that show nothing. They still cost
    // memory, so they must count against the budget and be evicted.
    let budget = 10_000;
    let mut r = ScreenReplay::with_budget(20, 10, budget);
    r.feed(b"head\r\n");
    for _ in 0..1_000 {
        r.feed(b"\x1b[999S");
    }
    r.feed(b"tail");
    let out = r.finish();
    assert!(out.head_dropped);
    assert!(
        out.row_count() <= budget / emulator::ROW_OVERHEAD_CELLS + 10,
        "{}",
        out.row_count()
    );
    assert!(out.to_plain().ends_with("tail"));
    assert!(!out.to_plain().contains("head"));
}

#[test]
fn a_wide_character_on_a_one_column_screen_does_not_panic() {
    for bytes in ["\x1b[?7l中x", "中\x1b[b", "中x\x1b[3b", "\x1b[?7l中\x1b[5b"] {
        let out = replay(1, 3, bytes.as_bytes());
        let _ = (out.to_plain(), out.to_ansi());
    }
    assert_eq!(plain(1, 3, "\x1b[?7l中x".as_bytes()), "x");
}

#[test]
fn needs_screen_replay_detects_vertical_motion_only() {
    assert!(!stream_needs_screen_replay(
        b"plain\r\n\x1b[31mred\x1b[0m\r\n"
    ));
    assert!(!stream_needs_screen_replay(b"50%\r\x1b[K60%\x1b[2G\x1b[1C"));
    assert!(!stream_needs_screen_replay(
        b"\x1b[?25l\x1b[>1u\x1b[?u\x1b]8;;x\x07"
    ));
    assert!(!stream_needs_screen_replay(b"\x1b[0 q unfinished \x1b["));
    assert!(stream_needs_screen_replay(b"a\r\n\x1b[1A\x1b[2K"));
    assert!(stream_needs_screen_replay(b"\x1b[5;1H"));
    assert!(stream_needs_screen_replay(b"\x1b[1;7r"));
    assert!(stream_needs_screen_replay(b"\x1bM"));
    assert!(stream_needs_screen_replay(b"\x1b[2J"));
    assert!(stream_needs_screen_replay(CODEX_16ROWS));
    assert!(stream_needs_screen_replay(&synth_ink(3)));
    // CUD, RIS, DECSED and a C1 (U+009B) CSI move or clear the screen too.
    assert!(stream_needs_screen_replay(b"a\x1b[2Bb"));
    assert!(stream_needs_screen_replay(b"old\x1bcnew"));
    assert!(stream_needs_screen_replay(b"\x1b[?2J"));
    assert!(stream_needs_screen_replay("\u{9b}1;1HX".as_bytes()));
    // Other private CSIs and a stray 0x9B continuation byte do not.
    assert!(!stream_needs_screen_replay(b"\x1b[?1049h\x1b[?2K"));
    assert!(!stream_needs_screen_replay("\u{29b}x".as_bytes()));
    assert!(!stream_needs_screen_replay(b"\x9b1;1H"));
}

#[test]
fn resync_skips_a_cut_sequence_or_character() {
    assert_eq!(resync_ring_head(b"8;29Hrest\x1b[1mx"), b"\x1b[1mx");
    assert_eq!(resync_ring_head(b"tail of line\nnext\x1b[m"), b"next\x1b[m");
    assert_eq!(resync_ring_head(b"\xb8\xadtext"), b"text");
    assert_eq!(resync_ring_head(b"\x1b[m"), b"\x1b[m");
    assert_eq!(resync_ring_head(b""), b"");
}

#[test]
fn degenerate_geometry_is_clamped() {
    let out = replay(0, 0, b"ab");
    assert_eq!((out.to_plain().as_str(), out.row_count()), ("ab", 2));
    let out = ScreenReplay::with_budget(10, 3, 0);
    let mut out = out;
    out.feed(b"1\r\n2\r\n3\r\n4\r\n5");
    let out = out.finish();
    assert!(out.head_dropped);
    assert_eq!(out.to_plain(), "3\n4\n5");
}

/// `cargo test --release --lib screen_replay::tests::replays_8_mib_quickly -- --ignored --nocapture`
#[test]
#[ignore]
fn replays_8_mib_quickly() {
    let mut stream = Vec::new();
    let codex = synth_codex(2_000, 40, 6);
    let ink = synth_ink(5_000);
    while stream.len() < 8 << 20 {
        stream.extend_from_slice(&codex);
        stream.extend_from_slice(&ink);
        stream.extend_from_slice(CODEX_16ROWS);
    }
    stream.truncate(8 << 20);
    let started = std::time::Instant::now();
    let mut replay = ScreenReplay::new(120, 40);
    for chunk in stream.chunks(32 << 10) {
        replay.feed(chunk);
    }
    let out = replay.finish();
    let replayed = started.elapsed();
    let plain = out.to_plain();
    let ansi = out.to_ansi();
    let total = started.elapsed();
    eprintln!(
        "8 MiB: replay {replayed:?}, replay+serialise {total:?}, {} rows, {} plain bytes, {} ansi bytes, head_dropped={}",
        out.row_count(),
        plain.len(),
        ansi.len(),
        out.head_dropped
    );
    if !cfg!(debug_assertions) {
        assert!(replayed.as_millis() < 100, "{replayed:?}");
    }

    // Worst cases for the side tables: a distinct truecolour pen and a
    // distinct hyperlink every few cells (compaction must keep this linear).
    let mut hostile = Vec::new();
    let mut n = 0u32;
    while hostile.len() < 8 << 20 {
        let _ = write!(
            StrBuf(&mut hostile),
            "\x1b[38;2;{};{};{}mab\x1b]8;;https://x.test/{n}\x1b\\cd\x1b]8;;\x1b\\{}",
            n & 255,
            (n >> 8) & 255,
            (n >> 16) & 255,
            if n % 30 == 29 { "\r\n" } else { "" }
        );
        n += 1;
    }
    let started = std::time::Instant::now();
    let mut replay = ScreenReplay::new(120, 40);
    replay.feed(&hostile);
    let out = replay.finish();
    eprintln!(
        "8 MiB of distinct pens and links: {:?}, {} rows",
        started.elapsed(),
        out.row_count()
    );
}

// ---- differential test against real libvte -------------------------------

/// Normalises a text export for comparison: trailing blanks per line and
/// leading/trailing blank lines are presentation details that VTE's export
/// and `to_plain` legitimately treat differently (VTE exports the rows up to
/// its cursor, keeps written trailing spaces, and writes never-written cells
/// inside a row as NUL where `to_plain` writes a space).
fn normalise(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|l| {
            l.replace('\0', " ")
                .trim_end_matches([' ', '\t'])
                .to_string()
        })
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let first = lines
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(lines.len());
    lines.drain(..first);
    lines
}

struct DiffCase {
    name: String,
    bytes: Vec<u8>,
    cols: usize,
    rows: usize,
    /// VTE-side resizes: (byte offset, cols, rows). The replay runs at the
    /// last geometry, like an app that only knows the final winsize.
    resizes: Vec<(usize, usize, usize)>,
}

fn diff_case(name: &str, cols: usize, rows: usize, bytes: impl Into<Vec<u8>>) -> DiffCase {
    DiffCase {
        name: name.to_string(),
        bytes: bytes.into(),
        cols,
        rows,
        resizes: Vec::new(),
    }
}

fn differential_cases() -> Vec<DiffCase> {
    let lines: String = (0..30).map(|i| format!("line {i}\r\n")).collect();
    let mut cases = vec![
        diff_case("codex-16rows", 120, 16, CODEX_16ROWS),
        DiffCase {
            resizes: vec![(17884, 120, 41), (46533, 122, 41)],
            ..diff_case("codex-40-resize", 120, 40, CODEX_40_RESIZE)
        },
        DiffCase {
            resizes: vec![(1833, 118, 41)],
            ..diff_case("claude-classic", 118, 41, CLAUDE_CLASSIC)
        },
        DiffCase {
            resizes: vec![(1774, 120, 41)],
            ..diff_case("kimi-40-resize", 120, 40, KIMI_40_RESIZE)
        },
        diff_case("synth-codex", 120, 40, synth_codex(150, 40, 6)),
        diff_case("synth-codex-small", 60, 12, synth_codex(80, 12, 4)),
        diff_case("synth-ink", 120, 40, synth_ink(300)),
        diff_case("scrolling", 20, 5, lines.clone()),
        diff_case("wrap", 10, 4, "0123456789X\r\n0123456789\r\nabc中文字符宽度测试end"),
        diff_case("pending-wrap-edits", 10, 4, "0123456789\x1b[K\r\n0123456789\x1b[DZ\r\nabcdefghij\x1b[1@"),
        diff_case("nowrap", 10, 4, "\x1b[?7l0123456789XYZ\r\nabc"),
        diff_case("cjk-cha", 20, 4, "中文ab\x1b[10GX\r\n中文\x1b[2G文\r\nab\x1b[1;4H中"),
        diff_case("combining", 20, 4, "e\u{301}x\x1b[4GY\r\n\u{301}start\r\n0123456789012345678e\u{301}\u{302}"),
        diff_case("region-ri", 10, 4, "a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[2;1H\x1bMX"),
        diff_case("region-lf", 10, 4, "a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[3;1H\nY\x1b[r"),
        diff_case("region-top", 10, 5, format!("{lines}\x1b[1;3r\x1b[3;1H\nX\nY\nZ\x1b[r")),
        diff_case("region-bad", 10, 4, "ab\x1b[3;3rc\x1b[0;0rd\x1b[;r\x1b[4;99rX"),
        diff_case("il-dl", 10, 5, "a\r\nb\r\nc\r\nd\x1b[2;1H\x1b[L\x1b[4;1H\x1b[2M\x1b[1;1H\x1b[M"),
        diff_case("su-sd", 10, 5, "a\r\nb\r\nc\r\nd\r\ne\x1b[2S\x1b[3T\x1b[2;4r\x1b[S"),
        diff_case("ich-dch-ech-rep", 12, 3, "abcdef\x1b[1;2H\x1b[2@\r\nabcdef\x1b[2;2H\x1b[2P\r\nabcdef\x1b[3;2H\x1b[2Xx\x1b[3b"),
        diff_case("save-restore", 10, 4, "a\x1b7\r\nb\x1b8c\x1b[s\r\n\r\nd\x1b[ue\x1b[31m\x1b7\x1b[m\x1b8x"),
        diff_case("el-ed", 10, 4, "abcdef\x1b[3D\x1b[1K\r\nabc\r\ndef\x1b[2K\r\nxyz\x1b[1D\x1b[1J\x1b[4;1Hlast\x1b[4;3H\x1b[J"),
        diff_case("ed3", 10, 4, format!("{lines}\x1b[3Jafter")),
        diff_case("cursor-moves", 20, 6, "\x1b[3;5Ha\x1b[2Ab\x1b[5Bc\x1b[3Cd\x1b[10De\x1b[2Ef\x1b[3Fg\x1b[7Gh\x1b[4di\x1b[15`j\x1b[99;99Hk\x1b[0;0Hl"),
        diff_case("hpr-vpr", 20, 5, "q\x1b[5aX\x1b[2eY"),
        diff_case("tabs", 30, 4, "a\tb\tc\r\n\tx\x1b[2Iy\x1b[Zz\r\nabc\x1b[3g\x1bH\x1b[1;1H\tT\x1b[3;3H\x1bH\r\t!"),
        diff_case("origin", 20, 6, "\x1b[3;5r\x1b[?6h\x1b[1;1Ho\x1b[9;1Hp\x1b[?6lq"),
        diff_case("irm", 10, 3, "abcdef\x1b[1;2H\x1b[4hXY\x1b[4lZ"),
        diff_case("dec-graphics", 20, 3, "\x1b(0lqqk\x1b(B\r\n\x0eqx\x0fqx\r\n\x1b)0\x0elqk\x0f"),
        diff_case("strings", 20, 3, "a\x1b]0;title\x07b\x1bP1$r0m\x1b\\c\x1b_apc\x1b\\d\x1b^pm\x1b\\e\x1bXsos\x1b\\f\x1b]8;;http://x\x1b\\g\x1b]8;;\x1b\\"),
        diff_case("unknown", 20, 3, "a\x1b[>4;1mb\x1b[?25lc\x1b[12$pd\x1b[5 qe\x1b#8f\x1b%Gg\x1b[?2026hh\x1b[=5ui\x1b[<1u"),
        diff_case("invalid-utf8", 20, 3, b"a\xffb\xe4\xb8c\xed\xa0\x80d\xf0\x9f\x98e\xc0\xafz".to_vec()),
        diff_case("c0-controls", 20, 3, "a\x07b\x08\x08c\x0bd\x0ce\x00f\x7fg\x1a\x18h"),
        diff_case("alt-screen", 20, 4, "before\r\n\x1b[?1049halt\x1b[2;1Hscreen\x1b[?1049lafter\r\n\x1b[?47hx\x1b[?47ly\x1b[?1047hz\x1b[?1047lw"),
        diff_case("decstr-ris", 20, 4, "\x1b[2;3r\x1b[?7l\x1b[!pabc\x1b[31mred\x1bcfresh"),
        diff_case("wide-edge", 5, 4, "abcd中\r\nab中cd\x1b[1;3Hx\r\n\x1b[3;5H中"),
        diff_case("bg-erase", 10, 3, "\x1b[41m\x1b[K\x1b[m\r\n\x1b[44mab\x1b[2Jx"),
        diff_case("reverse-index-scrollback", 10, 4, format!("{lines}\x1b[H\x1bM\x1bMtop")),
        diff_case("nel-ind", 10, 3, "a\x1bDb\x1bEc\u{85}d\x1bDe\x1bDf"),
    ];
    for c in &mut cases {
        if c.name.starts_with("codex-40") {
            // The replay knows only the last winsize the child saw.
            c.cols = 122;
            c.rows = 41;
        }
        if c.name.starts_with("kimi") {
            c.rows = 41;
        }
    }
    cases
}

/// Cases whose ED 2 makes the replay differ from VTE by design (VTE scrolls
/// the cleared screen into history); for those only the replay's lines must
/// be a suffix of VTE's.
fn uses_ed2(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|w| w == b"\x1b[2J")
}

/// Differential check against real libvte 0.76 through
/// tests/fixtures/screen_replay/vte_export.py. Needs python3-gi with Vte 3.91
/// and a display: run it through the headless wrapper, e.g.
/// `headless-gtk.sh cargo test --lib screen_replay::tests::vte_differential_matches_real_vte -- --ignored --exact`.
/// One libvte input: bytes, starting cols and rows, and resizes
/// `(byte offset, cols, rows)`.
type VteInput<'a> = (&'a [u8], usize, usize, &'a [(usize, usize, usize)]);

/// Feeds each `(bytes, cols, rows, resizes)` to a real libvte through
/// vte_export.py and returns its text exports in order.
fn vte_exports(tag: &str, inputs: &[VteInput<'_>]) -> Vec<String> {
    let dir = std::env::temp_dir().join(format!("screen-replay-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut manifest = String::new();
    for (i, (bytes, cols, rows, resizes)) in inputs.iter().enumerate() {
        let path = dir.join(format!("case{i}.bin"));
        std::fs::write(&path, bytes).unwrap();
        let _ = write!(manifest, "{} {cols} {rows}", path.display());
        for (off, c, r) in *resizes {
            let _ = write!(manifest, " {off}:{c}:{r}");
        }
        manifest.push('\n');
    }
    let manifest_path = dir.join("manifest");
    std::fs::write(&manifest_path, manifest).unwrap();
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/screen_replay/vte_export.py"
    );
    let output = std::process::Command::new("python3")
        .arg(script)
        .arg("--batch")
        .arg(&manifest_path)
        .output()
        .expect("run python3");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "vte_export.py failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut rest = &output.stdout[..];
    let mut exports = Vec::new();
    for _ in inputs {
        let newline = rest.iter().position(|&b| b == b'\n').unwrap();
        let len: usize = std::str::from_utf8(&rest[..newline])
            .unwrap()
            .parse()
            .unwrap();
        exports.push(String::from_utf8_lossy(&rest[newline + 1..newline + 1 + len]).into_owned());
        rest = &rest[newline + 1 + len..];
    }
    exports
}

/// Differential check against real libvte 0.76 through
/// tests/fixtures/screen_replay/vte_export.py. Needs python3-gi with Vte 3.91
/// and a display: run it through the headless wrapper, e.g.
/// `headless-gtk.sh cargo test --lib screen_replay::tests::vte_differential_matches_real_vte -- --ignored --exact`.
#[test]
#[ignore]
fn vte_differential_matches_real_vte() {
    let cases = differential_cases();
    let inputs: Vec<_> = cases
        .iter()
        .map(|case| {
            // Every capture with resizes started at ptycap.py's default 120x40.
            let (cols, rows) = if case.resizes.is_empty() {
                (case.cols, case.rows)
            } else {
                (120, 40)
            };
            (&case.bytes[..], cols, rows, &case.resizes[..])
        })
        .collect();
    let exports = vte_exports("diff", &inputs);
    let mut failures = Vec::new();
    for (case, vte) in cases.iter().zip(&exports) {
        let ours = normalise(&plain(case.cols, case.rows, &case.bytes));
        let theirs = normalise(vte);
        let ok = if uses_ed2(&case.bytes) {
            theirs.ends_with(&ours)
        } else {
            theirs == ours
        };
        if !ok {
            let at = ours
                .iter()
                .zip(&theirs)
                .position(|(a, b)| a != b)
                .unwrap_or(ours.len().min(theirs.len()));
            failures.push(format!(
                "{}: {} vs {} lines, first difference at line {at}:\n  ours: {:?}\n  vte:  {:?}",
                case.name,
                ours.len(),
                theirs.len(),
                ours.get(at),
                theirs.get(at)
            ));
        } else {
            eprintln!("{}: {} lines match", case.name, ours.len());
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The card side: `to_ansi` fed to a real libvte of the replay's width must
/// export the same logical lines as `to_plain` (soft wraps rejoined, so the
/// card can reflow). Tabs VTE could not store as tabs come back as spaces, so
/// whitespace runs are compared collapsed. Same wrapper as above.
#[test]
#[ignore]
fn to_ansi_round_trips_through_real_vte() {
    let cases = differential_cases();
    let replays: Vec<Replay> = cases
        .iter()
        .map(|case| replay(case.cols, case.rows, &case.bytes))
        .collect();
    let ansi: Vec<String> = replays.iter().map(Replay::to_ansi).collect();
    let inputs: Vec<_> = cases
        .iter()
        .zip(&ansi)
        .map(|(case, ansi)| (ansi.as_bytes(), case.cols, case.rows, &[][..]))
        .collect();
    let exports = vte_exports("ansi", &inputs);
    let collapse = |lines: Vec<String>| -> Vec<String> {
        lines
            .into_iter()
            .map(|l| {
                l.split([' ', '\t'])
                    .filter(|w| !w.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    };
    let mut failures = Vec::new();
    for ((case, out), vte) in cases.iter().zip(&replays).zip(&exports) {
        let ours = collapse(normalise(&out.to_plain()));
        let theirs = collapse(normalise(vte));
        if ours != theirs {
            failures.push(format!(
                "{}:\n  ours: {ours:?}\n  vte:  {theirs:?}",
                case.name
            ));
        } else {
            eprintln!("{}: {} lines match", case.name, ours.len());
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

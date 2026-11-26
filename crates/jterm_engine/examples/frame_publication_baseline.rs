//! Compare terminal feed, capture and delta costs on fixed public workloads.

use jterm_engine::terminal::Terminal;
use std::{hint::black_box, time::Instant};

const WARMUP: usize = 48;
const SAMPLES: usize = 400;

fn terminal(rows: usize, cols: usize) -> Terminal {
    let mut terminal = Terminal::new(rows, cols).unwrap();
    terminal.enable_underline_colors();
    terminal.enable_grapheme_clusters();
    for row in 1..=rows {
        terminal
            .feed(format!("\x1b[{row};1H\x1b[4;58;5;33mpublic text e\u{301} 界\x1b[0m").as_bytes())
            .unwrap();
    }
    terminal
}

fn measure(rows: usize, cols: usize, name: &str, inputs: [&[u8]; 2], expected: Option<usize>) {
    let mut terminal = terminal(rows, cols);
    let mut previous = terminal.frame().unwrap();
    for index in 0..(WARMUP + SAMPLES) {
        let input = inputs[index % inputs.len()];
        let start = Instant::now();
        black_box(terminal.feed(black_box(input)).unwrap());
        let fed = Instant::now();
        let frame = black_box(terminal.frame().unwrap());
        let captured = Instant::now();
        let changed_rows = (frame.stamp() != previous.stamp())
            .then(|| black_box(previous.delta_to(&frame).unwrap()).rows.len());
        let delta_done = Instant::now();
        assert_eq!(changed_rows, expected, "{name}");
        previous = frame;
        if index >= WARMUP {
            println!(
                "{rows},{cols},{name},{},{},{},{},{}",
                index - WARMUP,
                (fed - start).as_nanos(),
                (captured - fed).as_nanos(),
                (delta_done - captured).as_nanos(),
                changed_rows.map_or(-1, |count| count as i32),
            );
        }
    }
}

fn main() {
    println!("rows,cols,scenario,sample,feed_ns,capture_ns,delta_ns,changed_rows");
    for (rows, cols) in [(40, 120), (128, 256)] {
        measure(rows, cols, "query_only", [b"\x1b[6n"; 2], None);
        measure(
            rows,
            cols,
            "cursor_only",
            [b"\x1b[1;1H", b"\x1b[1;2H"],
            Some(0),
        );
        measure(
            rows,
            cols,
            "single_row",
            [b"\x1b[3;9Hx", b"\x1b[3;9Hy"],
            Some(1),
        );
    }
}

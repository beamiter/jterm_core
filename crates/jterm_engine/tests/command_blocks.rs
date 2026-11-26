use jterm_engine::{
    block_contract::{CompletedBlockOutcome, CompletionProvenance},
    command_blocks::OutputUnavailable,
    terminal::Terminal,
};

const START: &[u8] =
    b"\x1b]133;A\x07PROMPT\x1b]133;B\x07ECHOED COMMAND\r\n\x1b]133;C;id=one;seq=1;cmdline_url=printf%20sample;cwd_url=%2Ftmp\x07";
const END: &[u8] = b"\x1b]133;D;7;id=one\x1b\\";

#[test]
fn submitting_a_line_revokes_recall_until_the_next_complete_prompt() {
    let mut terminal = Terminal::new(3, 16).unwrap();
    terminal.observe_command_blocks();
    terminal.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07").unwrap();
    assert_eq!(terminal.command_prompt_generation(), Some(1));
    terminal.observe_prompt_input(b"printf hello");
    assert!(terminal.command_prompt_ready());
    assert_eq!(terminal.command_prompt_generation(), Some(1));
    terminal.observe_prompt_input(b"\r");
    assert!(!terminal.command_prompt_ready());
    assert_eq!(terminal.command_prompt_generation(), None);
    terminal
        .feed(b"\x1b]133;C;id=one;cmdline_url=printf%20hello\x07hello\r\n")
        .unwrap();
    assert!(terminal.command_running());
    terminal.feed(b"\x1b[?1049h").unwrap();
    assert!(terminal.alternate_screen_active());
    assert_eq!(terminal.command_prompt_generation(), None);
    terminal.feed(b"\x1b[?1049l").unwrap();
    assert!(!terminal.alternate_screen_active());
    terminal.feed(END).unwrap();
    assert!(!terminal.command_running());
    assert_eq!(terminal.completed_command_blocks().unwrap().len(), 1);
    terminal.feed(b"\x1b]133;A\x07$ ").unwrap();
    assert!(!terminal.command_prompt_ready());
    terminal.feed(b"\x1b]133;B\x07").unwrap();
    assert!(terminal.command_prompt_ready());
    assert_eq!(terminal.command_prompt_generation(), Some(2));
}

#[test]
fn real_feed_markers_survive_every_fragment_boundary_without_capturing_prompt_or_styles() {
    let packet = [
        START,
        b"\x1b]7;file://localhost/tmp\x1b\\\x1b]1337;CurrentDir=/tmp\x07",
        "\x1b[31m输出\x1b[0m\r\nsecond\r\n".as_bytes(),
        END,
        b"\x1b]133;A\x07NEXT PROMPT",
    ]
    .concat();
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(3, 8).unwrap();
        terminal.observe_command_blocks();
        for bytes in [&packet[..split], &packet[split..]] {
            assert!(terminal.feed(bytes).unwrap().is_empty());
        }
        let blocks = terminal.completed_command_blocks().unwrap();
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        assert_eq!(block.sequence, 1);
        assert_eq!(block.shell_sequence, Some(1));
        assert_eq!(block.execution_id, "one");
        assert_eq!(block.command, "printf sample");
        assert_eq!(block.cwd.as_deref(), Some("/tmp"));
        assert_eq!(block.output, Ok("输出\nsecond\n".into()));
        assert_eq!(block.outcome, CompletedBlockOutcome::Failed(7));
        assert_eq!(block.provenance, CompletionProvenance::ShellReported);
        let source = block.output_source.as_ref().unwrap();
        let end = packet.len() - END.len() - b"\x1b]133;A\x07NEXT PROMPT".len();
        assert_eq!(source.bytes, START.len() as u64..end as u64);
        assert_eq!((source.start_event, source.end_event), (3, 4));
        assert_eq!(source.start_size, (8, 3));
        assert_eq!(source.end_size, (8, 3));
        assert!(!source.resized);
    }
}

#[test]
fn disabled_observation_and_replaced_or_unmatched_records_stay_inert() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal.feed(&[START, END].concat()).unwrap();
    assert!(terminal.completed_command_blocks().is_none());
    terminal.observe_command_blocks();
    let stream = [
        b"\x1b]133;C;id=one;command=bare\x07\x1b]133;D;0;id=one\x07".as_slice(),
        START,
        b"\x1b]133;D;0;id=two\x07",
        END,
        START,
        b"\x1b]133;A\x07",
        END,
        b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C;id=one;command=x;cmd_truncated=1\x07",
        END,
    ];
    terminal.feed(&stream.concat()).unwrap();
    assert!(terminal.completed_command_blocks().unwrap().is_empty());

    terminal
        .feed(&[START, b"\x1b]133;D;id=one\x07"].concat())
        .unwrap();
    let record = &terminal.completed_command_blocks().unwrap()[0];
    assert_eq!(record.outcome, CompletedBlockOutcome::Unknown);
    assert_eq!(record.output, Ok(String::new()));
}

#[test]
fn fullscreen_cursor_rewrites_and_large_outputs_leave_bounded_status_records() {
    let mut terminal = Terminal::new(3, 8).unwrap();
    terminal.observe_command_blocks();
    for output in [
        b"prefix\x1b[?47hPRIVATE\x1b[?47lpostfix".as_slice(),
        b"overwrite\rnew",
        b"overwrite\x1b[2Knew",
    ] {
        terminal.feed(&[START, output, END].concat()).unwrap();
    }
    terminal.feed(START).unwrap();
    terminal.resize(3, 9).unwrap();
    terminal.feed(END).unwrap();
    assert!(terminal
        .completed_command_blocks()
        .unwrap()
        .iter()
        .all(|block| block.output == Err(OutputUnavailable::ScreenOperation)));

    terminal.feed(START).unwrap();
    let oversized = vec![b'x'; 16 * 1024 + 1];
    terminal.feed(&oversized).unwrap();
    terminal.feed(END).unwrap();
    let records = terminal.completed_command_blocks().unwrap();
    let resized = records[3].output_source.as_ref().unwrap();
    assert_eq!(resized.start_size, (8, 3));
    assert_eq!(resized.end_size, (9, 3));
    assert!(resized.resized);
    let limited = &records[4];
    assert_eq!(limited.output, Err(OutputUnavailable::ByteLimit));
    let bytes = &limited.output_source.as_ref().unwrap().bytes;
    assert_eq!(bytes.end - bytes.start, oversized.len() as u64);
    for _ in 0..10 {
        terminal.feed(&[START, b"text", END].concat()).unwrap();
    }
    let retained = terminal.completed_command_blocks().unwrap();
    assert_eq!(retained.len(), 8);
    assert_eq!(retained.front().unwrap().sequence, 8);
    assert_eq!(retained.back().unwrap().sequence, 15);
    assert!(retained
        .iter()
        .all(|block| block.output.as_deref() == Ok("text")));
}

#[test]
fn sanctioned_alternate_excursion_preserves_only_main_output_across_every_feed_split() {
    let bytes = [
        START,
        b"PRE\r\n\x1b[?1049h\x1b[2JSECRET\x1b]133;D;99;id=one\x07\x1b[?1049lPOST\r\n",
        END,
    ]
    .concat();
    for split in 0..=bytes.len() {
        let mut terminal = Terminal::new(4, 20).unwrap();
        terminal.observe_command_blocks();
        terminal.feed(&bytes[..split]).unwrap();
        terminal.feed(&bytes[split..]).unwrap();
        let blocks = terminal.completed_command_blocks().unwrap();
        assert_eq!(blocks.len(), 1, "split {split}");
        assert_eq!(blocks[0].output, Ok("PRE\nPOST\n".into()), "split {split}");
        assert_eq!(blocks[0].outcome, CompletedBlockOutcome::Failed(7));
    }
}

#[test]
fn non_saving_alternate_and_main_screen_operations_remain_unavailable() {
    for output in [
        b"PRE\x1b[?47hSECRET\x1b[?47lPOST".as_slice(),
        b"PRE\x1b[?1047hSECRET\x1b[?1047lPOST",
        b"PRE\x1b[2KPOST",
        b"PRE\rREWRITE",
        b"PRE\x1b[?1049hSECRET\x1b[?47lPOST",
    ] {
        let mut terminal = Terminal::new(4, 20).unwrap();
        terminal.observe_command_blocks();
        terminal.feed(&[START, output, END].concat()).unwrap();
        assert_eq!(
            terminal.completed_command_blocks().unwrap()[0].output,
            Err(OutputUnavailable::ScreenOperation)
        );
    }
}

#[test]
fn real_vim9_cursor_saved_excursion_retains_main_tags_not_the_fullscreen_ui() {
    let mut terminal = Terminal::new(24, 80).unwrap();
    terminal.observe_command_blocks();
    terminal.feed(START).unwrap();
    terminal.feed(b"BLOCK_PRE_TAG\r\n").unwrap();
    for chunk in include_bytes!("fixtures/vim9-command-alt-excursion.bin").chunks(7) {
        terminal.feed(chunk).unwrap();
    }
    terminal.feed(b"BLOCK_POST_TAG\r\n").unwrap();
    terminal.feed(END).unwrap();
    let blocks = terminal.completed_command_blocks().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(
        blocks[0].output,
        Ok("BLOCK_PRE_TAG\nBLOCK_POST_TAG\n".into())
    );
    assert_eq!(blocks[0].outcome, CompletedBlockOutcome::Failed(7));
}

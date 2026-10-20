//! Directory/title metadata must remain independent of full-screen lifecycle.
//! These are protocol guards, not a reproduction of an IDE auto-launch bug.
use jterm_core::parser::{Parser, ParserEvent};

fn metadata(path: &str, title: &str, terminator: &str) -> Vec<u8> {
    let encoded: String = path
        .bytes()
        .map(|byte| match byte {
            b'/' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect();
    format!(
        "\x1b]7;file://remote.example{encoded}{terminator}\x1b]1337;CurrentDir={path}{terminator}\x1b]2;{title}{terminator}"
    )
    .into_bytes()
}

fn no_mode_transition(events: &[ParserEvent]) {
    assert!(
        events
            .iter()
            .all(|event| matches!(event, ParserEvent::Bytes(_))),
        "display-only metadata produced semantic events: {events:?}"
    );
}

#[test]
fn cwd_and_titles_are_display_only_at_every_read_boundary() {
    for path in ["/tmp/plain", "/tmp/vim", "/tmp/nvim codex", "/tmp/空 格/雪"] {
        for terminator in ["\x07", "\x1b\\"] {
            // JSON event names are title text, never bridge lifecycle frames.
            let stream = metadata(path, r#"vim {"event":"hello","protocol":1}"#, terminator);
            for split in 0..=stream.len() {
                let mut parser = Parser::new();
                let mut events = Vec::new();
                parser.feed(&stream[..split], &mut events);
                parser.feed(&stream[split..], &mut events);
                no_mode_transition(&events);
                let forwarded: Vec<u8> = events
                    .iter()
                    .flat_map(|event| match event {
                        ParserEvent::Bytes(bytes) => bytes.clone(),
                        _ => unreachable!(),
                    })
                    .collect();
                // OSC 7 is normalized to ST; other metadata uses BEL. Both
                // spellings must retain exactly the original packet payloads.
                assert_eq!(
                    String::from_utf8(forwarded)
                        .unwrap()
                        .replace('\x07', "\x1b\\"),
                    String::from_utf8(metadata(
                        path,
                        r#"vim {"event":"hello","protocol":1}"#,
                        "\x1b\\"
                    ))
                    .unwrap()
                );
                assert!(!parser.bracketed_paste());
            }
        }
    }
}

#[test]
fn editor_exit_then_repeated_cd_never_reenters_fullscreen() {
    let mut parser = Parser::new();
    let mut events = Vec::new();
    parser.feed(b"\x1b[?1049hVim\x1b[?1049l", &mut events);
    assert!(events.contains(&ParserEvent::AltScreenEnter(1049)));
    assert!(events.contains(&ParserEvent::AltScreenLeave(1049)));
    events.clear();
    for index in 0..100 {
        let path = if index % 2 == 0 {
            "/tmp/vim"
        } else {
            "/tmp/雪 nvim codex"
        };
        let stream = metadata(path, "jsh: cwd", "\x07");
        for byte in stream.chunks(1) {
            parser.feed(byte, &mut events);
        }
    }
    no_mode_transition(&events);
    // An actual subsequent terminal lifecycle still works.
    events.clear();
    parser.feed(b"\x1b[?1049h\x1b[?1049l", &mut events);
    assert_eq!(
        events,
        [
            ParserEvent::AltScreenEnter(1049),
            ParserEvent::AltScreenLeave(1049)
        ]
    );
}

#[test]
fn interleaved_tabs_keep_partial_metadata_and_modes_separate() {
    let streams = [
        metadata("/tmp/vim", "vim", "\x1b\\"),
        metadata("/srv/空 格/nvim codex", "remote Vim9", "\x07"),
    ];
    let mut parsers = [Parser::new(), Parser::new()];
    let mut events = [Vec::new(), Vec::new()];
    for offset in 0..streams.iter().map(Vec::len).max().unwrap() {
        for tab in 0..2 {
            if let Some(byte) = streams[tab].get(offset) {
                parsers[tab].feed(std::slice::from_ref(byte), &mut events[tab]);
            }
        }
    }
    for (tab, tab_events) in events.iter().enumerate() {
        no_mode_transition(tab_events);
        let output: Vec<u8> = tab_events
            .iter()
            .flat_map(|event| match event {
                ParserEvent::Bytes(bytes) => bytes.clone(),
                _ => unreachable!(),
            })
            .collect();
        let title = if tab == 0 { "2;vim" } else { "2;remote Vim9" };
        assert!(String::from_utf8(output).unwrap().contains(title));
    }
    parsers[1].feed(b"\x1b[?1049h", &mut events[1]);
    assert!(events[1].contains(&ParserEvent::AltScreenEnter(1049)));
    no_mode_transition(&events[0]);
}

#[test]
fn shell_cd_lifecycle_and_remote_session_are_not_editor_lifecycle() {
    let stream = concat!(
        "\x1b]7770;remote-session\x07",
        "\x1b]133;A\x07$ \x1b]133;B\x07cd -\r\n",
        "\x1b]133;C;cmdline_url=cd%20-;cwd_url=%2Ftmp%2Fvim\x07",
        "\x1b]7;file://remote.example/tmp/nvim%20codex\x07",
        "\x1b]133;D;0;cwd_url=%2Ftmp%2Fnvim%20codex\x07",
        "\x1b]2;vim nvim codex\x07\x1b]133;A\x07$ \x1b]133;B\x07"
    );
    for width in 1..=stream.len() {
        let mut parser = Parser::new();
        let mut events = Vec::new();
        for chunk in stream.as_bytes().chunks(width) {
            parser.feed(chunk, &mut events);
        }
        assert!(
            events.iter().all(|event| matches!(
                event,
                ParserEvent::Bytes(_)
                    | ParserEvent::RemoteSessionId(_)
                    | ParserEvent::PromptStart
                    | ParserEvent::PromptEnd
                    | ParserEvent::CommandStart(_)
                    | ParserEvent::CommandEnd { .. }
            )),
            "chunk width {width}: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ParserEvent::CommandStart(_)))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ParserEvent::CommandEnd { .. }))
                .count(),
            1
        );
    }
}

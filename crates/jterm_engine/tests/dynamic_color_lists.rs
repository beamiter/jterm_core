use jterm_engine::{
    colors::{ColorState, DynamicColorState, TerminalDefaultColors},
    semantics::{Effect, Effects},
    terminal::Terminal,
};

fn wire(effects: Effects) -> Vec<u8> {
    effects
        .into_iter()
        .flat_map(|effect| match effect {
            Effect::Reply(bytes) => bytes,
            _ => panic!("unexpected color effect"),
        })
        .collect()
}

const EXPECTED: &[u8] =
    b"\x1b]10;rgb:1111/2222/3333\x07\x1b]11;rgb:4444/5555/6666\x07\x1b]12;rgb:7777/8888/9999\x07";

#[test]
fn hosted_dynamic_lists_survive_every_feed_split() {
    let packet = b"\x1b]10;#112233;#445566;#778899\x07\x1b]10;?;?;?\x07";
    for split in 0..=packet.len() {
        let mut terminal = Terminal::new(3, 12).unwrap();
        terminal.set_color_defaults(TerminalDefaultColors::default());
        let frame = terminal.frame().unwrap();
        let mut reply = terminal.feed(&packet[..split]).unwrap();
        reply.extend(terminal.feed(&packet[split..]).unwrap());
        assert_eq!(reply, EXPECTED);
        let after = terminal.frame().unwrap();
        assert_eq!(after.rows(), frame.rows());
        assert_eq!(after.cursor(), frame.cursor());
    }
}

#[test]
fn both_color_profiles_apply_successive_slots_and_preserve_reset_semantics() {
    let defaults = TerminalDefaultColors::default();
    let mut ember = ColorState::new(defaults);
    let mut frost = DynamicColorState::default();
    for profile in 0..2 {
        let mut apply = |command, value, terminator: &[u8]| {
            if profile == 0 {
                wire(ember.apply_ember_osc(command, value, terminator))
            } else {
                wire(frost.apply_frost_osc(command, value, terminator, defaults))
            }
        };
        assert!(apply("10", "#112233;#445566;#778899", b"\x07").is_empty());
        assert_eq!(apply("10", "?;?;?;?", b"\x07"), EXPECTED);
        assert!(apply("10", "invalid;;#aabbcc", b"\x07").is_empty());
        assert_eq!(
            apply("11", "?;?", b"\x1b\\"),
            b"\x1b]11;rgb:4444/5555/6666\x1b\\\x1b]12;rgb:aaaa/bbbb/cccc\x1b\\"
        );
        assert_eq!(
            apply("10", "#010203;?;#040506", b"\x07"),
            b"\x1b]11;rgb:4444/5555/6666\x07"
        );
        apply("110", "", b"\x07");
        assert_eq!(apply("10", "?", b"\x07"), b"\x1b]10;rgb:ffff/ffff/ffff\x07");
        assert_eq!(apply("11", "?", b"\x07"), b"\x1b]11;rgb:4444/5555/6666\x07");
    }
}

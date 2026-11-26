//! Terminfo for the shared emulator's interactive shell profile.

use std::{fs::File, io::Read, sync::OnceLock};

pub const NAME: &str = "jterm-256color";
const DATABASE: &str = "/usr/share/terminfo";
const ENTRY: &str = "/usr/share/terminfo/j/jterm-256color";

/// Compiled from `terminfo/jterm-256color.src`. Install the same bytes for
/// programs that open their terminfo database directly.
pub const COMPILED_ENTRY: &[u8] = include_bytes!("../terminfo/jterm-256color.bin");

/// Set terminal identification in the child. An unpackaged Linux build or
/// standalone SSH helper carries the same entry for ncurses callers.
pub fn environment() -> [(&'static str, &'static str); 4] {
    static TERMINFO: OnceLock<String> = OnceLock::new();
    let database = TERMINFO.get_or_init(|| {
        if installed_entry_matches() {
            DATABASE.to_owned()
        } else {
            hex_entry()
        }
    });
    [
        ("TERM", NAME),
        ("TERMINFO", database),
        ("COLORTERM", ""),
        ("VTE_VERSION", ""),
    ]
}

fn installed_entry_matches() -> bool {
    let Ok(file) = File::open(ENTRY) else {
        return false;
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let mut bytes = Vec::new();
    file.take(COMPILED_ENTRY.len() as u64 + 1)
        .read_to_end(&mut bytes)
        .is_ok()
        && bytes == COMPILED_ENTRY
}

fn hex_entry() -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut entry = String::with_capacity(4 + COMPILED_ENTRY.len() * 2);
    entry.push_str("hex:");
    for &byte in COMPILED_ENTRY {
        entry.push(char::from(DIGITS[usize::from(byte >> 4)]));
        entry.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use jterm_engine::{frame::Screen, terminal::Terminal, Color};
    use std::{fs, path::Path, process::Command};

    #[test]
    #[ignore = "requires the ncurses tic, infocmp and tput tools"]
    fn compiled_profile_matches_source_and_engine() {
        let database = std::env::temp_dir().join(format!("jterm-profile-{}", std::process::id()));
        fs::create_dir(&database).unwrap();
        struct RemoveDatabase(std::path::PathBuf);
        impl Drop for RemoveDatabase {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let directory = RemoveDatabase(database);
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("terminfo/jterm-256color.src");
        assert!(Command::new("tic")
            .args(["-x", "-o"])
            .arg(&directory.0)
            .arg(source)
            .status()
            .unwrap()
            .success());
        let inline = hex_entry();
        let dump = |database: &std::ffi::OsStr| {
            let output = Command::new("infocmp")
                .args(["-1", "-x", NAME])
                .env("TERMINFO", database)
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output);
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .filter(|line| !line.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(dump(directory.0.as_os_str()), dump(inline.as_ref()));

        let cap = |arguments: &[&str]| {
            let result = Command::new("tput")
                .args(arguments)
                .env("TERMINFO", &inline)
                .env("TERM", NAME)
                .output()
                .unwrap();
            assert!(result.status.success(), "{arguments:?}: {result:?}");
            result.stdout
        };
        assert_eq!(cap(&["colors"]), b"256\n");
        let mut terminal = Terminal::new(24, 80).unwrap();
        terminal.enable_device_answers();
        terminal.feed(&cap(&["smkx"])).unwrap();
        for (key, capability) in [
            ("ArrowUp", "kcuu1"),
            ("ArrowDown", "kcud1"),
            ("ArrowLeft", "kcub1"),
            ("ArrowRight", "kcuf1"),
            ("Home", "khome"),
            ("End", "kend"),
            ("Backspace", "kbs"),
            ("Insert", "kich1"),
            ("Delete", "kdch1"),
            ("PageUp", "kpp"),
            ("PageDown", "knp"),
            ("F1", "kf1"),
            ("F2", "kf2"),
            ("F3", "kf3"),
            ("F4", "kf4"),
            ("F5", "kf5"),
            ("F6", "kf6"),
            ("F7", "kf7"),
            ("F8", "kf8"),
            ("F9", "kf9"),
            ("F10", "kf10"),
            ("F11", "kf11"),
            ("F12", "kf12"),
        ] {
            assert_eq!(
                terminal.encode_key(key, false, false, false).unwrap(),
                cap(&[capability])
            );
        }
        terminal.feed(&cap(&["rmkx"])).unwrap();
        assert_eq!(
            terminal.encode_key("ArrowUp", false, false, false).unwrap(),
            b"\x1b[A"
        );
        terminal.feed(&cap(&["smcup"])).unwrap();
        terminal.feed(&cap(&["cup", "1", "2"])).unwrap();
        terminal.feed(&cap(&["setaf", "202"])).unwrap();
        terminal.feed(&cap(&["setab", "32"])).unwrap();
        terminal.feed(b"x").unwrap();
        let frame = terminal.frame().unwrap();
        assert_eq!(frame.stamp().screen, Screen::Alternate);
        let cell = frame.rows()[1].cells[2];
        assert_eq!(cell.character, 'x');
        assert_eq!(cell.foreground, Color::Indexed(202));
        assert_eq!(cell.background, Color::Indexed(32));
        assert_eq!(
            terminal.feed(b"\x1b[c\x1b[6n").unwrap(),
            b"\x1b[?1;2c\x1b[2;4R"
        );
        terminal.feed(&cap(&["rmcup"])).unwrap();
        assert_eq!(terminal.frame().unwrap().stamp().screen, Screen::Primary);
        terminal.feed(&cap(&["BE"])).unwrap();
        assert_eq!(
            terminal.encode_paste("text").unwrap(),
            [cap(&["PS"]), b"text".to_vec(), cap(&["PE"])].concat()
        );
        terminal.feed(&cap(&["BD"])).unwrap();
        assert_eq!(terminal.encode_paste("text").unwrap(), b"text");
        assert_eq!(terminal.unsupported_count(), 0);
    }
}

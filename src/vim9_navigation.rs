//! Shared Quick Open and Go to line input policy. Locations are data, never Ex
//! commands; resolving a query only reads the immutable project index.

use super::{Vim9Command, Vim9ProjectFile, Vim9ProjectSnapshot};

const MAX_QUERY_CHARS: usize = 256;
const MAX_POSITION: u64 = i32::MAX as u64;

/// Parse a one-based line and optional UTF-8 byte column. Whitespace around
/// numbers is allowed; signs, zero, overflow and command text are not.
pub fn parse_vim9_location(value: &str) -> Result<(u64, Option<u64>), &'static str> {
    fn number(value: &str) -> Option<u64> {
        let value = value.trim();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        value
            .parse::<u64>()
            .ok()
            .filter(|number| (1..=MAX_POSITION).contains(number))
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err("Enter a single-line position, for example 42:8.");
    }
    let mut parts = value.trim().split(':');
    let line = parts
        .next()
        .and_then(number)
        .ok_or("Enter a line from 1 to 2147483647, optionally followed by :byte-column.")?;
    let column = parts
        .next()
        .map(|part| number(part).ok_or("The byte column must be from 1 to 2147483647."))
        .transpose()?;
    if parts.next().is_some() {
        return Err("Use line or line:byte-column, for example 42:8.");
    }
    Ok((line, column))
}

/// A validated picker query, tied to the index snapshot used to interpret
/// literal colon filenames. Cache it with the matching rows, and reconcile the
/// input before activation so a delayed event cannot use an earlier location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vim9QuickOpenQuery {
    pub file_query: String,
    pub location: Option<(u64, Option<u64>)>,
    pub current_buffer: bool,
}

impl Vim9QuickOpenQuery {
    /// Build a typed navigation action. Current-buffer jumps work for unnamed
    /// and modified buffers and do not depend on the project index or a path.
    pub fn command(&self, file: Option<&Vim9ProjectFile>) -> Result<Vim9Command, &'static str> {
        if self.current_buffer {
            let (line, column) = self.location.ok_or("Enter a line or line:byte-column.")?;
            return Ok(Vim9Command::Goto { line, column });
        }
        let path = file
            .ok_or("Choose a matching project file")?
            .path
            .to_str()
            .ok_or("The filename is not valid UTF-8")?;
        let (line, column) = self
            .location
            .map_or((None, None), |(line, column)| (Some(line), column));
        Ok(Vim9Command::Open {
            path: path.to_owned(),
            line,
            column,
        })
    }
}

impl Vim9ProjectSnapshot {
    /// Resolve `path[:line[:byte-column]]` or `:line[:byte-column]` without I/O.
    /// An exact indexed filename (relative, absolute or basename) takes
    /// precedence over location syntax, preserving real colon filenames.
    /// Exact absolute paths are converted to the relative text used by fuzzy
    /// ranking; external/unindexed paths remain the explicit Open action's job.
    pub fn quick_open_query(&self, value: &str) -> Result<Vim9QuickOpenQuery, &'static str> {
        if value.contains(['\r', '\n', '\0']) {
            return Err("Enter a single-line filename or position.");
        }
        let value = value.trim();
        if value.chars().count() > MAX_QUERY_CHARS {
            return Err("Quick Open accepts up to 256 characters.");
        }
        let normalize = |query: &str| {
            self.files
                .iter()
                .find(|file| file.path.to_str() == Some(query))
                .map_or_else(
                    || query.strip_prefix("./").unwrap_or(query).to_owned(),
                    |file| file.relative.clone(),
                )
        };
        let literal = normalize(value);
        if self.files.iter().any(|file| {
            file.relative == literal
                || file.path.file_name().and_then(|s| s.to_str()) == Some(value)
        }) {
            return Ok(Vim9QuickOpenQuery {
                file_query: literal,
                location: None,
                current_buffer: false,
            });
        }
        if let Some(position) = value.strip_prefix(':') {
            return Ok(Vim9QuickOpenQuery {
                file_query: String::new(),
                location: Some(parse_vim9_location(position)?),
                current_buffer: true,
            });
        }
        let Some((prefix, last)) = value.rsplit_once(':') else {
            return Ok(Vim9QuickOpenQuery {
                file_query: literal,
                location: None,
                current_buffer: false,
            });
        };
        let prefix_query = normalize(prefix.trim());
        if self.files.iter().any(|file| {
            file.relative == prefix_query
                || file.path.file_name().and_then(|s| s.to_str()) == Some(prefix.trim())
        }) {
            return Ok(Vim9QuickOpenQuery {
                file_query: prefix_query,
                location: Some(parse_vim9_location(last)?),
                current_buffer: false,
            });
        }
        // Only a numeric-looking penultimate segment is a line number. Other
        // colons remain in the path, e.g. `src/a:b.rs:42`. Include incomplete
        // and signed numbers here so they report errors instead of opening a
        // different file at the trailing number.
        let (path, position) = match prefix.rsplit_once(':') {
            Some((path, line))
                if line.trim().is_empty()
                    || line.trim().starts_with(['+', '-'])
                    || line.trim().bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                (path, &value[path.len() + 1..])
            }
            _ => (prefix, last),
        };
        if path.trim().is_empty() {
            return Err("Enter a filename before :line, or use :line for the current buffer.");
        }
        Ok(Vim9QuickOpenQuery {
            file_query: normalize(path.trim()),
            location: Some(parse_vim9_location(position)?),
            current_buffer: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    fn project(names: &[&str]) -> Vim9ProjectSnapshot {
        Vim9ProjectSnapshot {
            files: Arc::new(
                names
                    .iter()
                    .map(|name| Vim9ProjectFile {
                        path: Path::new("/project").join(name),
                        relative: (*name).into(),
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn shared_positions_reject_invalid_or_executable_text() {
        assert_eq!(parse_vim9_location(" 42 : 8 "), Ok((42, Some(8))));
        assert_eq!(parse_vim9_location("2147483647"), Ok((MAX_POSITION, None)));
        assert_eq!(parse_vim9_location("001:002"), Ok((1, Some(2))));
        for invalid in [
            "",
            "0",
            "-1",
            "+2",
            "2:",
            ":2",
            "2:0",
            "2:3:4",
            "２",
            "1|quit",
            "1:2\n",
            "2147483648",
            "1:2147483648",
            "18446744073709551616",
        ] {
            assert!(parse_vim9_location(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn fuzzy_and_absolute_targets_carry_typed_locations() {
        let project = project(&["src/main.rs", "文档/数据 |%.rs", "src/a:b.rs"]);
        for (text, name, line, column) in [
            ("src/mrs:42:8", "src/main.rs", 42, Some(8)),
            ("/project/src/main.rs:20", "src/main.rs", 20, None),
            ("./src/main.rs: 7 : 3", "src/main.rs", 7, Some(3)),
            ("数据:3:4", "文档/数据 |%.rs", 3, Some(4)),
            ("src/a:b.rs:2", "src/a:b.rs", 2, None),
        ] {
            let parsed = project.quick_open_query(text).unwrap();
            assert_eq!(parsed.location, Some((line, column)));
            assert!(!parsed.current_buffer);
            let matches = project.matches(&parsed.file_query, 20);
            assert_eq!(matches[0].relative, name);
            let Vim9Command::Open {
                path,
                line: actual_line,
                column: actual_column,
            } = parsed.command(matches.first()).unwrap()
            else {
                panic!("expected Open")
            };
            assert_eq!(path, format!("/project/{name}"));
            assert_eq!(actual_line, Some(line));
            assert_eq!(actual_column, column);
        }
    }

    #[test]
    fn current_buffer_navigation_does_not_need_files_or_a_successful_index() {
        let project = Vim9ProjectSnapshot {
            scanning: true,
            error: Some("rg unavailable".into()),
            ..Default::default()
        };
        for value in [":42", ":42:8", " : 42 : 8 "] {
            let parsed = project.quick_open_query(value).unwrap();
            assert!(parsed.current_buffer);
            assert!(parsed.file_query.is_empty());
            assert!(matches!(
                parsed.command(None),
                Ok(Vim9Command::Goto { line: 42, .. })
            ));
        }
    }

    #[test]
    fn exact_colon_filenames_win_over_location_syntax() {
        let project = project(&["src/main.rs:42", "src/main.rs", ":12", "notes:0", "a:1:2"]);
        for value in [
            "src/main.rs:42",
            "main.rs:42",
            "./src/main.rs:42",
            "/project/src/main.rs:42",
            ":12",
            "notes:0",
            "a:1:2",
        ] {
            let parsed = project.quick_open_query(value).unwrap();
            assert!(!parsed.current_buffer, "{value}");
            assert_eq!(parsed.location, None, "{value}");
            let matches = project.matches(&parsed.file_query, 20);
            assert!(matches!(
                parsed.command(matches.first()),
                Ok(Vim9Command::Open {
                    line: None,
                    column: None,
                    ..
                })
            ));
        }
        let colon = project.quick_open_query("src/main.rs:42:9").unwrap();
        assert_eq!(colon.file_query, "src/main.rs:42");
        assert_eq!(colon.location, Some((9, None)));
    }

    #[test]
    fn invalid_suffixes_and_unmatched_queries_cannot_dispatch() {
        let project = project(&["src/main.rs"]);
        for invalid in [
            ":",
            ":0",
            ":1:",
            "::1",
            "src/main.rs:",
            "src/main.rs:0",
            "src/main.rs:-1",
            "src/main.rs:+1",
            "src/main.rs:1:0",
            "src/main.rs:1:2147483648",
            "src/main.rs:1:2|quit",
            "src/main.rs::2",
            "src/main.rs:0:2",
            "src/main.rs:1\n",
            "src/main.rs:1\0",
            "src/main.rs:abc",
        ] {
            assert!(project.quick_open_query(invalid).is_err(), "{invalid:?}");
        }
        assert!(project.quick_open_query(&"界".repeat(257)).is_err());
        assert!(project.quick_open_query(&"界".repeat(256)).is_ok());
        let query = project.quick_open_query("missing:8").unwrap();
        assert!(query
            .command(project.matches(&query.file_query, 20).first())
            .is_err());
    }
}

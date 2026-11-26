use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use gtk4::glib;
use pcre2::bytes::{Regex, RegexBuilder};

use crate::viewport::ScrollTarget;
use crate::{EmbeddedTerminal, FindFrame, FindMatch, FindRegion, Pty, TerminalPane};

const MAX_QUERY_BYTES: usize = 8192;
const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;
const MAX_MATCHES: usize = 16 * 1024;
const SEARCH_BUDGET: Duration = Duration::from_millis(20);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TerminalFindStatus {
    #[default]
    Idle,
    Results {
        current: usize,
        total: usize,
        truncated: bool,
    },
    Error(String),
}

#[derive(Default)]
struct SearchState {
    query: Option<Regex>,
    frame: Option<FindFrame>,
    origin: Option<ScrollTarget>,
    matches: Rc<[FindMatch]>,
    current: Option<usize>,
    truncated: bool,
    status: TerminalFindStatus,
}

type StatusHandler = Rc<dyn Fn(TerminalFindStatus)>;

/// Find in the same published live grid and retained history that the pane paints.
/// The controller owns only query, projection and highlight state.
pub struct TerminalFind<P: Pty> {
    source: EmbeddedTerminal<P>,
    state: RefCell<SearchState>,
    handler: RefCell<Option<StatusHandler>>,
    refresh_pending: Cell<bool>,
}

impl<P: Pty> TerminalFind<P> {
    pub(crate) fn new(pane: Rc<TerminalPane<P>>) -> Self {
        Self {
            source: EmbeddedTerminal::for_find(pane),
            state: RefCell::default(),
            handler: RefCell::default(),
            refresh_pending: Cell::new(false),
        }
    }

    pub fn set_status_handler(&self, handler: impl Fn(TerminalFindStatus) + 'static) {
        self.handler.replace(Some(Rc::new(handler)));
    }

    pub fn set_query(&self, query: &str, use_regex: bool, case_sensitive: bool) {
        self.clear();
        if query.is_empty() {
            return;
        }
        if query.len() > MAX_QUERY_BYTES {
            self.report(TerminalFindStatus::Error(
                "Find query exceeds 8 KiB.".into(),
            ));
            return;
        }
        let query = compile_query(query, use_regex, case_sensitive);
        match query {
            Ok(query) => {
                {
                    let mut state = self.state.borrow_mut();
                    state.query = Some(query);
                    state.origin = self.source.find_origin();
                }
                self.rebuild();
            }
            Err(error) => self.report(TerminalFindStatus::Error(format!(
                "Invalid search pattern: {error}"
            ))),
        }
    }

    pub fn next(&self) {
        self.navigate(false);
    }

    pub fn previous(&self) {
        self.navigate(true);
    }

    pub fn clear(&self) {
        self.source.clear_find();
        let status = self.state.borrow().status.clone();
        self.state.replace(SearchState {
            status,
            ..SearchState::default()
        });
        self.report(TerminalFindStatus::Idle);
    }

    pub fn status(&self) -> TerminalFindStatus {
        self.state.borrow().status.clone()
    }

    pub(crate) fn observe_output(self: &Rc<Self>) {
        if self.state.borrow().query.is_none() || self.refresh_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(40), move || {
            if let Some(controller) = weak.upgrade() {
                controller.refresh_pending.set(false);
                if controller.state.borrow().query.is_some() && !controller.is_current() {
                    controller.rebuild();
                }
            }
        });
    }

    fn is_current(&self) -> bool {
        self.state
            .borrow()
            .frame
            .as_ref()
            .is_some_and(|frame| self.source.find_is_current(frame))
    }

    fn navigate(&self, previous: bool) {
        if self.state.borrow().query.is_none() {
            return;
        }
        if !self.is_current() {
            self.rebuild();
            if !self.is_current() {
                return;
            }
        }
        let mut state = self.state.borrow_mut();
        let count = state.matches.len();
        if count == 0 {
            return;
        }
        let current = state.current.unwrap_or(0);
        state.current = Some(if previous {
            (current + count - 1) % count
        } else {
            (current + 1) % count
        });
        drop(state);
        self.reveal();
    }

    fn rebuild(&self) {
        let Some(query) = self.state.borrow().query.clone() else {
            return;
        };
        let deadline = Instant::now() + SEARCH_BUDGET;
        let origin = self.state.borrow().origin;
        let Some(snapshot) = self.source.find_snapshot_at(
            FindRegion::Transcript,
            origin,
            MAX_CAPTURE_BYTES,
            &mut || Instant::now() >= deadline,
        )
        else {
            self.source.clear_find();
            self.state.borrow_mut().frame = None;
            self.report(TerminalFindStatus::Error(
                "Terminal output is updating. Retry Find.".into(),
            ));
            return;
        };
        let deadline = Instant::now() + SEARCH_BUDGET;
        let mut matches = Vec::new();
        let mut truncated = snapshot.incomplete;
        'windows: for window in &snapshot.windows {
            for found in query.find_iter(window.text().as_bytes()) {
                let found = match found {
                    Ok(found) if found.start() != found.end() => found,
                    Ok(_) => continue,
                    Err(error) => {
                        self.source.clear_find();
                        self.state.borrow_mut().frame = None;
                        self.report(TerminalFindStatus::Error(format!(
                            "Search stopped: {error}"
                        )));
                        return;
                    }
                };
                if matches.len() >= MAX_MATCHES || Instant::now() >= deadline {
                    truncated = true;
                    break 'windows;
                }
                if let Some(found) = window.match_at(found.start()..found.end()) {
                    matches.push(found);
                }
            }
        }
        let mut state = self.state.borrow_mut();
        let current = state
            .current
            .and_then(|index| {
                let previous = state.matches.get(index)?;
                let old = &state.frame.as_ref()?.0;
                let new = &snapshot.frame.0;
                (old.history_start == new.history_start
                    && old.stamp.epoch == new.stamp.epoch
                    && old.stamp.layout == new.stamp.layout
                    && old.stamp.screen == new.stamp.screen)
                    .then(|| {
                        matches.iter().position(|found| {
                            found.start == previous.start && found.end == previous.end
                        })
                    })
                    .flatten()
            })
            .or_else(|| {
                state
                    .current
                    .filter(|_| !matches.is_empty())
                    .map(|index| index.min(matches.len() - 1))
            })
            .or_else(|| (!matches.is_empty()).then_some(0));
        state.frame = Some(snapshot.frame);
        state.matches = matches.into();
        state.current = current;
        state.truncated = truncated;
        drop(state);
        self.reveal();
    }

    fn reveal(&self) {
        let state = self.state.borrow();
        let Some(frame) = &state.frame else { return };
        let visible = state.current.map_or_else(
            || {
                self.source
                    .highlight_find(frame, state.matches.clone(), None)
            },
            |current| self.source.show_find(frame, state.matches.clone(), current),
        );
        let status = if visible {
            TerminalFindStatus::Results {
                current: state.current.map_or(0, |index| index + 1),
                total: state.matches.len(),
                truncated: state.truncated,
            }
        } else {
            TerminalFindStatus::Error("Terminal output changed. Retry Find.".into())
        };
        drop(state);
        if matches!(status, TerminalFindStatus::Error(_)) {
            self.source.clear_find();
        }
        self.report(status);
    }

    fn report(&self, status: TerminalFindStatus) {
        let changed = self.state.borrow().status != status;
        self.state.borrow_mut().status = status.clone();
        if changed {
            let handler = self.handler.borrow().clone();
            if let Some(handler) = handler {
                handler(status);
            }
        }
    }
}

impl<P: Pty> Drop for TerminalFind<P> {
    fn drop(&mut self) {
        self.source.clear_find();
    }
}

fn compile_query(
    query: &str,
    use_regex: bool,
    case_sensitive: bool,
) -> Result<Regex, pcre2::Error> {
    let mut pattern = String::from("(*LIMIT_MATCH=100000)(*LIMIT_DEPTH=1000)(*LIMIT_HEAP=1024)");
    if use_regex {
        pattern.push_str(query);
    } else {
        for character in query.chars() {
            if matches!(
                character,
                '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
            ) {
                pattern.push('\\');
            }
            pattern.push(character);
        }
    }
    RegexBuilder::new()
        .utf(true)
        .multi_line(true)
        .caseless(!case_sensitive)
        .build(&pattern)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_patterns_keep_pcre2_lookaround_backreferences_case_and_literal_semantics() {
        let lookahead = compile_query("foo(?=bar)", true, true).unwrap();
        assert_eq!(lookahead.find(b"foobar").unwrap().unwrap().end(), 3);
        let backreference = compile_query(r"(word) \1", true, true).unwrap();
        assert!(backreference.is_match(b"word word").unwrap());
        assert!(compile_query("literal[.]", false, true)
            .unwrap()
            .is_match(b"literal[.]")
            .unwrap());
        assert!(!compile_query("Value", false, true)
            .unwrap()
            .is_match(b"value")
            .unwrap());
        assert!(compile_query("Value", false, false)
            .unwrap()
            .is_match(b"value")
            .unwrap());
        assert!(compile_query("[", true, true).is_err());
        let bounded = compile_query("(*NO_START_OPT)(a+)+$", true, true).unwrap();
        assert!(bounded
            .is_match(format!("{}!", "a".repeat(4096)).as_bytes())
            .is_err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires private GTK and a real runtime PTY; run alone"]
    fn runtime_find_reveals_live_and_archived_matches_and_keeps_the_source_during_output() {
        use crate::{LinkActions, PtyExit, RuntimeShell, Theme};
        use gtk4::{gdk, pango, prelude::*};
        use std::ffi::CString;

        struct RuntimeOnly;
        impl Pty for RuntimeOnly {
            fn write(&self, _: &[u8]) -> Result<(), String> {
                unreachable!("runtime owns input")
            }
            fn resize(&self, _: u16, _: u16) -> Result<(), String> {
                unreachable!("runtime owns resize")
            }
            fn stop(&self) {
                unreachable!("runtime owns stop")
            }
            fn start_reader(
                &self,
                _: impl FnMut(Vec<u8>) + 'static,
                _: impl FnOnce(PtyExit) + 'static,
            ) -> Result<(), String> {
                unreachable!("runtime owns read")
            }
        }
        gtk4::init().expect("GTK display");
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"printf 'archived needle\n'
n=0
while [ $n -lt 90 ]; do
    printf 'row %s\n' "$n"
    n=$((n+1))
done
printf 'live needle echo echo\n'
IFS= read -r first
printf 'appended after find\n'
IFS= read -r last"#
                .into(),
        ];
        let environment = [
            "TERM=xterm-256color",
            "PATH=/usr/bin:/bin",
            "LC_ALL=C.UTF-8",
        ]
        .into_iter()
        .map(|value| CString::new(value).unwrap())
        .collect();
        let runtime =
            RuntimeShell::spawn_program(&argv, &std::env::temp_dir(), environment).unwrap();
        let theme = Theme {
            font: pango::FontDescription::from_string("Monospace 10"),
            foreground: gdk::RGBA::WHITE,
            background: gdk::RGBA::BLACK,
            palette: crate::default_ansi_palette(),
            cursor: None,
        };
        let (widget, pane) = TerminalPane::<RuntimeOnly>::with_runtime(
            runtime.clone(),
            theme,
            |text, _| text.to_owned(),
            LinkActions {
                admit: |_, _| false,
                find_plain: None,
                attach_opener: |_| Box::new(|_| {}),
            },
            |_| {},
        )
        .unwrap();
        pane.set_history_row_limit(5000);
        let window = gtk4::Window::builder()
            .default_width(640)
            .default_height(320)
            .child(&widget)
            .build();
        window.present();
        let phase = Cell::new("initial output");
        let wait = |predicate: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(8);
            while !predicate() {
                assert!(
                    Instant::now() < deadline,
                    "find did not reveal its current source during {}",
                    phase.get()
                );
                context.iteration(false);
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        let screen_text = || {
            let buffer = pane.screen().buffer();
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string()
        };
        let tagged_text = || {
            let buffer = pane.screen().buffer();
            let tag = buffer.tag_table().lookup("jterm-screen-find-current")?;
            let mut start = buffer.start_iter();
            if !start.has_tag(&tag) && !start.forward_to_tag_toggle(Some(&tag)) {
                return None;
            }
            if !start.has_tag(&tag) {
                return None;
            }
            let mut end = start;
            end.forward_to_tag_toggle(Some(&tag));
            Some(buffer.text(&start, &end, true).to_string())
        };
        wait(&|| screen_text().contains("live needle"));
        let find = pane.find_controller();
        assert!(Rc::ptr_eq(&find, &pane.find_controller()));
        phase.set("initial literal query");
        find.set_query("needle", false, true);
        wait(&|| {
            find.status()
                == TerminalFindStatus::Results {
                    current: 1,
                    total: 2,
                    truncated: false,
                }
                && tagged_text().as_deref() == Some("needle")
                && screen_text().contains("live needle")
        });
        phase.set("archived Next");
        find.next();
        wait(&|| {
            find.status()
                == TerminalFindStatus::Results {
                    current: 2,
                    total: 2,
                    truncated: false,
                }
                && tagged_text().as_deref() == Some("needle")
                && screen_text().contains("archived needle")
        });
        phase.set("live Previous");
        find.previous();
        wait(&|| {
            tagged_text().as_deref() == Some("needle") && screen_text().contains("live needle")
        });
        find.next();
        wait(&|| {
            tagged_text().as_deref() == Some("needle") && screen_text().contains("archived needle")
        });
        phase.set("output while archived match is current");
        pane.write_input(b"continue\n".to_vec()).unwrap();
        wait(&|| {
            tagged_text().as_deref() == Some("needle")
                && screen_text().contains("archived needle")
                && matches!(find.status(), TerminalFindStatus::Results { total: 2, .. })
        });
        // Let the output refresh run before testing a different PCRE2 pattern.
        let deadline = Instant::now() + Duration::from_millis(150);
        while Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(tagged_text().as_deref(), Some("needle"));
        assert!(screen_text().contains("archived needle"));
        phase.set("lookahead query");
        find.set_query("echo(?= echo)", true, true);
        wait(&|| tagged_text().as_deref() == Some("echo") && screen_text().contains("live needle"));
        phase.set("backreference query");
        find.set_query(r"(echo) \1", true, true);
        wait(&|| tagged_text().as_deref() == Some("echo echo"));
        find.set_query("[", true, true);
        assert!(matches!(find.status(), TerminalFindStatus::Error(_)));
        assert!(tagged_text().is_none());
        phase.set("case-insensitive query");
        find.set_query("NEEDLE", false, false);
        wait(&|| matches!(find.status(), TerminalFindStatus::Results { total: 2, .. }));
        find.clear();
        assert_eq!(find.status(), TerminalFindStatus::Idle);
        assert!(tagged_text().is_none());
        window.destroy();
        phase.set("runtime stop");
        pane.stop();
        wait(&|| runtime.ready_to_discard());
    }
}

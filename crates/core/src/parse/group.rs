//! Entry grouping: which physical lines belong to one logical log entry.
//!
//! Two invariants the rest of the system depends on, both enforced here and asserted in
//! the tests:
//!
//! 1. **Every non-blank line belongs to exactly one group.** No orphans, no double counts.
//! 2. **Fetches return whole groups.** Asking for five entries can never hand back the
//!    bottom half of a stack trace with the header missing — which is the failure that
//!    makes a log tool useless to an agent.
//!
//! When the rules are unsure, the tie is broken toward **starting a new entry**.
//! Over-splitting is cosmetic: the agent gets eight rows instead of one and reads them
//! fine. Over-merging destroys the boundary that fingerprinting keys on, and silently
//! makes two different bugs look like one.

use crate::model::DelimiterMode;
use regex::Regex;
use std::sync::LazyLock;

/// Continuation markers, per runtime. Stack traces are the most rigidly structured thing
/// in a log stream, not the least, which is why this table is exact in practice rather
/// than heuristic.
pub static CONTINUATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        ^(?:
            \s+at\s                     # V8, Java, .NET
          | \s+File\s\x22                # Python
          | \s+\(\w[\w.]*\)\s            # Elixir / Erlang:  (my_app) lib/foo.ex:1: Foo.bar/1
          | \s+from\s                    # Ruby
          | \S+:\d+:in\s                 # Ruby backtrace, unindented
          | \t+\S                        # Go
          | \s+\.{3}\s                   # elided frames
          | \s+\^+\s*$                   # caret pointers under a source line
          | \s{2,}\S                     # generic indent
          | \s+---\s                     # Rust/anyhow cause chains
          | Caused\sby:                  # Java / anyhow
          | \s+\d+:\s+\S                 # Rust backtrace frame lines
        )",
    )
    .expect("CONTINUATION regex")
});

/// Openers that begin a multi-line block whose first line is not itself the identifier.
static BLOCK_OPENER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:Traceback \(most recent call last\):|The above exception was the direct cause|During handling of the above exception)")
        .expect("BLOCK_OPENER regex")
});

/// Column-zero line that terminates a Python traceback: `ValueError: boom`.
static PY_TERMINATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_.]*(?:Error|Exception|Warning|Exit|Interrupt|Fault)?:\s")
        .expect("PY_TERMINATOR regex")
});

pub fn is_continuation(line: &str) -> bool {
    !line.trim().is_empty() && CONTINUATION.is_match(line)
}

/// A closed group of lines, in line indices relative to the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub start_line: i64,
    pub end_line: i64,
    pub captured_at: i64,
}

/// One line as it arrives, already rendered.
#[derive(Debug, Clone)]
pub struct Line<'a> {
    pub index: i64,
    pub raw: &'a [u8],
    pub text: &'a str,
    pub captured_at: i64,
}

/// Incremental grouper. Feed lines in order; take closed groups as they are decided.
///
/// A group can only be closed once something after it proves it ended, so the last group
/// stays open until [`Grouper::flush`] — called at end of run, and by the daemon after a
/// short idle gap so the live view does not wait on a server that has gone quiet.
pub struct Grouper {
    mode: DelimiterMode,
    open: Option<Group>,
    /// Blank lines seen since the last content line. They join the current group only if
    /// the group turns out to continue; otherwise they belong to nothing.
    pending_blanks: i64,
    /// Inside a Python-style block whose terminator has not arrived yet.
    in_python_block: bool,
}

impl Grouper {
    pub fn new(mode: DelimiterMode) -> Grouper {
        Grouper {
            mode,
            open: None,
            pending_blanks: 0,
            in_python_block: false,
        }
    }

    pub fn set_mode(&mut self, mode: DelimiterMode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> DelimiterMode {
        self.mode
    }

    pub fn has_open(&self) -> bool {
        self.open.is_some()
    }

    /// Feed one line. Returns the group that this line closed, if any.
    pub fn push(&mut self, line: &Line<'_>) -> Option<Group> {
        if line.text.trim().is_empty() {
            // Blank lines are held, not attributed. A blank inside a stack trace belongs
            // to it; a blank between two entries belongs to neither.
            if self.open.is_some() {
                self.pending_blanks += 1;
            }
            return None;
        }

        let starts_new = self.starts_entry(line);

        if starts_new {
            let closed = self.open.take();
            self.pending_blanks = 0;
            self.in_python_block = BLOCK_OPENER.is_match(line.text);
            self.open = Some(Group {
                start_line: line.index,
                end_line: line.index,
                captured_at: line.captured_at,
            });
            closed
        } else {
            match self.open.as_mut() {
                Some(g) => {
                    // The held blanks are now interior, so they join.
                    g.end_line = line.index;
                    self.pending_blanks = 0;
                    if self.in_python_block && PY_TERMINATOR.is_match(line.text) {
                        self.in_python_block = false;
                    }
                    None
                }
                None => {
                    // A continuation with nothing to continue — the run began mid-trace,
                    // or the previous group was already flushed on an idle gap. Invariant
                    // 1 says it still has to land somewhere, so it opens its own group.
                    self.open = Some(Group {
                        start_line: line.index,
                        end_line: line.index,
                        captured_at: line.captured_at,
                    });
                    None
                }
            }
        }
    }

    /// Close whatever is open. Called at end of run and after an idle gap.
    pub fn flush(&mut self) -> Option<Group> {
        self.pending_blanks = 0;
        self.in_python_block = false;
        self.open.take()
    }

    fn starts_entry(&self, line: &Line<'_>) -> bool {
        // A Python traceback runs until its column-zero terminator, whatever the mode
        // says. This is the case that produced a real fingerprint collision in the
        // prototype, so it is handled structurally rather than left to the mode rules.
        if self.in_python_block {
            return false;
        }
        if is_continuation(line.text) {
            return false;
        }
        // A traceback opener matches no mode's start rule, so without this it merges into
        // whatever info line happened to precede it.
        if BLOCK_OPENER.is_match(line.text) {
            return true;
        }

        match self.mode {
            DelimiterMode::Json => {
                let t = line.text.trim_start();
                t.starts_with('{')
            }
            DelimiterMode::ColourBracket => super::mode::starts_with_sgr(line.raw),
            DelimiterMode::Prefix => super::mode::LINE_START.is_match(line.text),
            // Nothing matched, so the safe default applies: start a new entry.
            DelimiterMode::Fallback => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::vt;

    /// Group a whole stream and return groups plus the mode used.
    fn group_all(mode: DelimiterMode, raw: &[&[u8]]) -> Vec<Group> {
        let mut g = Grouper::new(mode);
        let mut out = Vec::new();
        for (i, r) in raw.iter().enumerate() {
            let text = vt::render_line(r);
            if let Some(closed) = g.push(&Line {
                index: i as i64,
                raw: r,
                text: &text,
                captured_at: i as i64,
            }) {
                out.push(closed);
            }
        }
        if let Some(last) = g.flush() {
            out.push(last);
        }
        out
    }

    /// Invariant 1: every non-blank line lands in exactly one group.
    fn assert_partition(raw: &[&[u8]], groups: &[Group]) {
        let mut owner: Vec<Option<usize>> = vec![None; raw.len()];
        for (gi, g) in groups.iter().enumerate() {
            for i in g.start_line..=g.end_line {
                let slot = &mut owner[i as usize];
                assert!(
                    slot.is_none(),
                    "line {i} claimed by group {gi} and group {}",
                    slot.unwrap()
                );
                *slot = Some(gi);
            }
        }
        for (i, r) in raw.iter().enumerate() {
            if !vt::render_line(r).trim().is_empty() {
                assert!(owner[i].is_some(), "orphaned line {i}: {:?}", vt::render_line(r));
            }
        }
    }

    /// Invariant 2: no group starts on a continuation line.
    fn assert_whole_groups(raw: &[&[u8]], groups: &[Group]) {
        for g in groups {
            let first = vt::render_line(raw[g.start_line as usize]);
            assert!(
                !is_continuation(&first),
                "group starts mid-trace on {:?}",
                first
            );
        }
    }

    const NODE: &[&[u8]] = &[
        b"GET /users",
        b"Error: boom one",
        b"    at Object.<anonymous> (/app/server.js:12:9)",
        b"    at Module._compile (node:internal/modules/cjs/loader:1105:14)",
        b"    at require (node:internal/modules/cjs/loader:1000:19)",
        b"GET /posts",
        b"Error: boom two",
        b"    at Object.<anonymous> (/app/other.js:4:1)",
    ];

    #[test]
    fn node_groups_on_stack_frames_alone() {
        // The hard case: no colour, no timestamp, no level. Only `^\s+at\s` to go on.
        let groups = group_all(DelimiterMode::Fallback, NODE);
        assert_eq!(groups.len(), 4, "{groups:#?}");
        assert_partition(NODE, &groups);
        assert_whole_groups(NODE, &groups);
        assert_eq!(groups[1].start_line, 1);
        assert_eq!(groups[1].end_line, 4);
    }

    const PYTHON: &[&[u8]] = &[
        b"INFO:root:GET /users",
        b"Traceback (most recent call last):",
        b"  File \"/app/main.py\", line 12, in handler",
        b"    raise RuntimeError(\"boom one\")",
        b"RuntimeError: boom one",
        b"INFO:root:GET /posts",
        b"Traceback (most recent call last):",
        b"  File \"/app/main.py\", line 20, in other",
        b"    raise ValueError(\"boom two\")",
        b"ValueError: boom two",
    ];

    #[test]
    fn python_tracebacks_stay_whole_through_their_terminator() {
        // The terminator sits at column zero and looks exactly like a new entry. Getting
        // this wrong splits the exception type off its own traceback.
        let groups = group_all(DelimiterMode::Prefix, PYTHON);
        assert_eq!(groups.len(), 4, "{groups:#?}");
        assert_partition(PYTHON, &groups);
        assert_whole_groups(PYTHON, &groups);
        assert_eq!((groups[1].start_line, groups[1].end_line), (1, 4));
        assert_eq!((groups[3].start_line, groups[3].end_line), (6, 9));
    }

    const ELIXIR: &[&[u8]] = &[
        b"\x1b[32m12:04:11.882 [info] GET /users\x1b[0m",
        b"\x1b[31m12:04:11.884 [error] ** (Ecto.NoResultsError) expected at least one result",
        b"    (my_app) lib/my_app/users.ex:42: Users.get_user!/1",
        b"    (my_app) lib/my_app_web/ctrl.ex:12: Ctrl.show/2\x1b[0m",
        b"\x1b[32m12:04:11.900 [info] GET /posts\x1b[0m",
    ];

    #[test]
    fn elixir_colour_bracketing_recovers_entries_exactly() {
        let groups = group_all(DelimiterMode::ColourBracket, ELIXIR);
        assert_eq!(groups.len(), 3, "{groups:#?}");
        assert_partition(ELIXIR, &groups);
        assert_whole_groups(ELIXIR, &groups);
        assert_eq!((groups[1].start_line, groups[1].end_line), (1, 3));
    }

    const RUBY: &[&[u8]] = &[
        b"I, [2026-07-26T12:04:11.882000 #1]  INFO -- : GET /users",
        b"E, [2026-07-26T12:04:11.883000 #1] ERROR -- : boom one",
        b"-e:5:in '<main>'",
        b"        from -e:2:in 'block'",
        b"I, [2026-07-26T12:04:11.884000 #1]  INFO -- : GET /posts",
    ];

    #[test]
    fn ruby_unindented_backtrace_lines_stay_attached() {
        // `-e:5:in '<main>'` sits at column zero. Without a rule for it the frame detaches
        // into its own entry — harmless but untidy, and the prototype caught it.
        let groups = group_all(DelimiterMode::Prefix, RUBY);
        assert_eq!(groups.len(), 3, "{groups:#?}");
        assert_partition(RUBY, &groups);
        assert_whole_groups(RUBY, &groups);
        assert_eq!((groups[1].start_line, groups[1].end_line), (1, 3));
    }

    #[test]
    fn blank_lines_between_entries_belong_to_neither() {
        let raw: &[&[u8]] = &[b"Error: one", b"    at a (x.js:1:1)", b"", b"Error: two"];
        let groups = group_all(DelimiterMode::Fallback, raw);
        assert_eq!(groups.len(), 2);
        assert_eq!((groups[0].start_line, groups[0].end_line), (0, 1));
        assert_eq!((groups[1].start_line, groups[1].end_line), (3, 3));
        assert_partition(raw, &groups);
    }

    #[test]
    fn a_stream_that_opens_mid_trace_still_partitions() {
        let raw: &[&[u8]] = &[b"    at Object.foo (a.js:1:1)", b"Error: later"];
        let groups = group_all(DelimiterMode::Fallback, raw);
        assert_partition(raw, &groups);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn go_tab_indented_frames_continue() {
        let raw: &[&[u8]] = &[
            b"panic: runtime error: index out of range [3]",
            b"\tmain.main()",
            b"\t\t/app/main.go:12 +0x1d",
            b"exit status 2",
        ];
        let groups = group_all(DelimiterMode::Fallback, raw);
        assert_eq!(groups.len(), 2, "{groups:#?}");
        assert_eq!((groups[0].start_line, groups[0].end_line), (0, 2));
        assert_partition(raw, &groups);
        assert_whole_groups(raw, &groups);
    }

    #[test]
    fn jsonl_gives_one_group_per_object() {
        let raw: &[&[u8]] = &[
            br#"{"level":"info","msg":"a"}"#,
            br#"{"level":"error","msg":"b"}"#,
            br#"{"level":"info","msg":"c"}"#,
        ];
        let groups = group_all(DelimiterMode::Json, raw);
        assert_eq!(groups.len(), 3);
        assert_partition(raw, &groups);
    }
}

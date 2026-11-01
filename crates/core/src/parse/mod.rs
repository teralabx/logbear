//! The capture pipeline.
//!
//! ```text
//! pty bytes → line split → vt emulation → mode detect → grouping → severity → fingerprint
//!            deterministic  deterministic   sampled      inferential   read     normalised
//! ```
//!
//! Grouping is the only inferential stage, and entries record that in their `Boundaries`.
//! Severity reads what the producer declared and reports `unknown` when nothing was
//! declared; it never infers a level from the shape of the text. Every other stage is
//! reproducible from the stored bytes, so the derived index can be dropped and rebuilt
//! whenever a rule changes.

pub mod classify;
pub mod fingerprint;
pub mod group;
pub mod mode;
pub mod severity;
pub mod vt;

use crate::model::{Boundaries, DelimiterMode, Level, MAX_RAW_LINE};
use crate::parse::severity::Evidence;

/// One physical line of the captured stream, verbatim.
#[derive(Debug, Clone)]
pub struct RawLine {
    /// Index within the run, from zero. Stable for the life of the run; entries point at it.
    pub index: i64,
    pub captured_at: i64,
    /// Bytes with the trailing newline removed, everything else intact, ANSI included.
    pub bytes: Vec<u8>,
}

/// A grouped, classified, fingerprinted entry, ready to be written.
#[derive(Debug, Clone)]
pub struct ParsedEntry {
    pub start_line: i64,
    pub end_line: i64,
    pub captured_at: i64,
    pub text: String,
    pub level: Level,
    /// Which rung of the severity ladder answered; `Unknown` is a valid answer.
    pub evidence: Evidence,
    pub is_error: bool,
    pub error_reason: Option<String>,
    pub template: String,
    pub hash: String,
    pub boundaries: Boundaries,
}

/// Splits a byte stream into lines on LF.
///
/// `ONLCR` on a pty turns every `\n` into `\r\n`, so a trailing `\r` is dropped as a
/// transport artifact.
#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
    next_index: i64,
    /// Arrival time of the first byte of the line being assembled, so a line's timestamp
    /// is when it started rather than when it completed.
    line_started_at: Option<i64>,
}

impl LineSplitter {
    pub fn new() -> LineSplitter {
        LineSplitter::default()
    }

    pub fn next_index(&self) -> i64 {
        self.next_index
    }

    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }

    pub fn push(&mut self, bytes: &[u8], captured_at: i64, out: &mut Vec<RawLine>) {
        for &b in bytes {
            if self.buf.is_empty() && self.line_started_at.is_none() {
                self.line_started_at = Some(captured_at);
            }
            if b == b'\n' {
                self.emit(out, captured_at);
            } else {
                self.buf.push(b);
                // A progress bar can emit megabytes of `\r` and never an LF, so cap the
                // buffer; vt emulation still collapses the result.
                if self.buf.len() >= MAX_RAW_LINE {
                    self.emit(out, captured_at);
                }
            }
        }
    }

    /// Emits whatever partial line is buffered.
    ///
    /// Called at end of run and on an idle gap, so a prompt that never prints a newline
    /// still becomes visible.
    pub fn flush(&mut self, captured_at: i64) -> Option<RawLine> {
        if self.buf.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        self.emit(&mut out, captured_at);
        out.pop()
    }

    fn emit(&mut self, out: &mut Vec<RawLine>, now: i64) {
        let mut bytes = std::mem::take(&mut self.buf);
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        out.push(RawLine {
            index: self.next_index,
            captured_at: self.line_started_at.unwrap_or(now),
            bytes,
        });
        self.next_index += 1;
        self.line_started_at = None;
    }
}

/// What one feed of bytes produced.
#[derive(Debug, Default)]
pub struct ParseOutput {
    /// Every line the feed completed, in order. These are the bytes to persist.
    pub lines: Vec<RawLine>,
    /// Entries whose boundaries are now settled.
    pub entries: Vec<ParsedEntry>,
    /// Set on the first feed that settles the mode, so the run row is updated once.
    pub mode_settled: Option<DelimiterMode>,
}

/// Per-run parser over the full pipeline.
pub struct Parser {
    splitter: LineSplitter,
    detector: mode::Detector,
    grouper: group::Grouper,
    /// Rendered text of lines not yet claimed by a closed group, keyed by line index, so
    /// closing a group does not require reading back from storage. Drained as groups
    /// close, so it only ever holds the entry currently open.
    pending: std::collections::VecDeque<(i64, String)>,
    mode: DelimiterMode,
    mode_locked: bool,
    /// Rendered text of lines seen before the mode settled — regrouped once it does.
    warmup: Vec<(RawLine, String)>,
    last_activity: i64,
    /// Applied to each line as soon as it is split out, before rendering, grouping,
    /// classification or fingerprinting. Redacting any later would leak the secret into
    /// the entry text and the fingerprint template.
    redactor: Option<std::sync::Arc<crate::redact::Redactor>>,
}

/// Silence after which an open group is closed, so the live view is not blocked by a
/// producer that goes quiet mid-entry. Well above the sub-millisecond gaps within a stack
/// trace, so it never splits one.
pub const IDLE_CLOSE_MICROS: i64 = 400_000;

impl Default for Parser {
    fn default() -> Self {
        Parser::new()
    }
}

impl Parser {
    pub fn new() -> Parser {
        Parser {
            splitter: LineSplitter::new(),
            detector: mode::Detector::default(),
            grouper: group::Grouper::new(DelimiterMode::Fallback),
            pending: std::collections::VecDeque::new(),
            mode: DelimiterMode::Fallback,
            mode_locked: false,
            warmup: Vec::new(),
            last_activity: 0,
            redactor: None,
        }
    }

    /// A parser that carries on a run already holding `next_index` lines.
    ///
    /// A run with lines has had its mode settled, so the mode is locked rather than
    /// sampled again from the tail, which would group the two halves under different rules.
    pub fn resume(next_index: i64, mode: DelimiterMode) -> Parser {
        let mut p = Parser::new();
        p.splitter.next_index = next_index;
        if next_index > 0 {
            p.mode = mode;
            p.mode_locked = true;
            p.grouper.set_mode(mode);
        }
        p
    }

    pub fn with_redactor(mut self, r: std::sync::Arc<crate::redact::Redactor>) -> Parser {
        self.redactor = Some(r);
        self
    }

    pub fn mode(&self) -> DelimiterMode {
        self.mode
    }

    /// Feed captured bytes.
    pub fn push(&mut self, bytes: &[u8], captured_at: i64) -> ParseOutput {
        let mut out = ParseOutput::default();
        let mut lines = Vec::new();
        self.splitter.push(bytes, captured_at, &mut lines);
        self.last_activity = captured_at;
        self.consume(lines, &mut out);
        out
    }

    /// Closes an entry that has gone quiet, returning nothing until [`IDLE_CLOSE_MICROS`]
    /// has elapsed since the last activity.
    pub fn tick(&mut self, now: i64) -> ParseOutput {
        let mut out = ParseOutput::default();
        if now - self.last_activity < IDLE_CLOSE_MICROS {
            return out;
        }
        // Surface a newline-less partial line (a prompt, a spinner).
        if let Some(line) = self.splitter.flush(now) {
            self.consume(vec![line], &mut out);
        }
        // Settle on whatever sample exists rather than holding entries back indefinitely;
        // the mode is derived, so a reparse can revise it.
        if !self.mode_locked {
            self.lock_mode(&mut out);
        }
        if let Some(g) = self.grouper.flush() {
            self.close(g, &mut out);
        }
        out
    }

    /// Ends the run: settles the mode, flushes every buffer and closes the last group.
    pub fn finish(&mut self, now: i64) -> ParseOutput {
        let mut out = ParseOutput::default();
        if let Some(line) = self.splitter.flush(now) {
            self.consume(vec![line], &mut out);
        }
        if !self.mode_locked {
            self.lock_mode(&mut out);
        }
        if let Some(g) = self.grouper.flush() {
            self.close(g, &mut out);
        }
        out
    }

    fn consume(&mut self, lines: Vec<RawLine>, out: &mut ParseOutput) {
        for mut line in lines {
            if let Some(r) = &self.redactor {
                r.redact(&mut line.bytes);
            }
            let text = vt::render_line(&line.bytes);

            if !self.mode_locked {
                self.detector.observe(&line.bytes, &text);
                self.warmup.push((line.clone(), text));
                out.lines.push(self.warmup.last().unwrap().0.clone());
                if self.detector.decided().is_some() {
                    self.lock_mode(out);
                }
                continue;
            }

            out.lines.push(line.clone());
            self.feed_grouper(&line, &text, out);
        }
    }

    /// Commits to a mode and replays the warmup lines through the grouper.
    ///
    /// Warmup lines are buffered rather than grouped on arrival so that the opening lines
    /// of a run are not grouped under the wrong rules.
    fn lock_mode(&mut self, out: &mut ParseOutput) {
        let mode = self.detector.settle();
        self.mode = mode;
        self.mode_locked = true;
        self.grouper.set_mode(mode);
        out.mode_settled = Some(mode);

        let warmup = std::mem::take(&mut self.warmup);
        for (line, text) in warmup {
            self.feed_grouper(&line, &text, out);
        }
    }

    fn feed_grouper(&mut self, line: &RawLine, text: &str, out: &mut ParseOutput) {
        // Buffered before grouping: the line that closes the previous group is the line
        // that opens the next one, and both need their text available.
        self.pending.push_back((line.index, text.to_string()));

        let closed = self.grouper.push(&group::Line {
            index: line.index,
            raw: &line.bytes,
            text,
            captured_at: line.captured_at,
        });

        if let Some(g) = closed {
            self.close(g, out);
        }
    }

    fn close(&mut self, g: group::Group, out: &mut ParseOutput) {
        let text = self.take_group_text(&g);
        if text.trim().is_empty() {
            return;
        }
        let c = classify::classify(&text);
        let sig = fingerprint::signature(&text, c.level);
        out.entries.push(ParsedEntry {
            start_line: g.start_line,
            end_line: g.end_line,
            captured_at: g.captured_at,
            text,
            level: c.level,
            evidence: c.evidence,
            is_error: c.is_error,
            error_reason: c.reason_string(),
            template: sig.template,
            hash: sig.hash,
            boundaries: self.mode.boundaries(),
        });
    }

    /// Slices the closed group's lines out of the pending buffer, discarding anything
    /// older — blank-line padding belonging to neither neighbour.
    fn take_group_text(&mut self, g: &group::Group) -> String {
        let mut kept: Vec<String> = Vec::new();
        while let Some((index, _)) = self.pending.front() {
            let index = *index;
            if index > g.end_line {
                break;
            }
            let (_, text) = self.pending.pop_front().expect("front exists");
            if index >= g.start_line {
                kept.push(text);
            }
        }
        kept.join("\n").trim_end().to_string()
    }
}

/// Rebuilds entries from stored lines, so a parser change can be applied to existing runs
/// without recapturing them.
pub fn reparse(lines: &[RawLine]) -> (DelimiterMode, Vec<ParsedEntry>) {
    let mut p = Parser::new();
    let mut entries = Vec::new();
    let mut last_at = 0;
    for line in lines {
        let mut buf = line.bytes.clone();
        buf.push(b'\n');
        let out = p.push(&buf, line.captured_at);
        entries.extend(out.entries);
        last_at = line.captured_at;
    }
    let out = p.finish(last_at);
    entries.extend(out.entries);
    (p.mode(), entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(stream: &[u8]) -> (DelimiterMode, Vec<RawLine>, Vec<ParsedEntry>) {
        let mut p = Parser::new();
        let mut lines = Vec::new();
        let mut entries = Vec::new();
        // One byte at a time: the result must not depend on read boundaries.
        for (i, b) in stream.iter().enumerate() {
            let out = p.push(&[*b], i as i64);
            lines.extend(out.lines);
            entries.extend(out.entries);
        }
        let out = p.finish(stream.len() as i64);
        lines.extend(out.lines);
        entries.extend(out.entries);
        (p.mode(), lines, entries)
    }

    const NODE: &[u8] = b"GET /users\nError: boom one\n    at Object.<anonymous> (/app/server.js:12:9)\n    at Module._compile (node:internal/modules/cjs/loader:1105:14)\nGET /posts\nError: boom two\n    at Object.<anonymous> (/app/other.js:4:1)\n";

    #[test]
    fn node_stream_groups_and_catches_both_errors() {
        let (mode, lines, entries) = feed(NODE);
        assert_eq!(mode, DelimiterMode::Fallback);
        assert_eq!(lines.len(), 7);
        assert_eq!(entries.len(), 4, "{entries:#?}");
        assert_eq!(entries.iter().filter(|e| e.is_error).count(), 2);
        // The stack trace is one entry, not four.
        assert_eq!(entries[1].start_line, 1);
        assert_eq!(entries[1].end_line, 3);
        assert!(entries[1].text.contains("at Module._compile"));
    }

    #[test]
    fn read_boundaries_do_not_affect_the_result() {
        let (_, _, one_byte) = feed(NODE);
        let mut p = Parser::new();
        let out = p.push(NODE, 0);
        let mut all = out.entries;
        all.extend(p.finish(1).entries);
        assert_eq!(one_byte.len(), all.len());
        for (a, b) in one_byte.iter().zip(all.iter()) {
            assert_eq!((a.start_line, a.end_line), (b.start_line, b.end_line));
            assert_eq!(a.hash, b.hash);
        }
    }

    #[test]
    fn crlf_from_the_pty_is_normalised_away() {
        let (_, lines, _) = feed(b"one\r\ntwo\r\n");
        assert_eq!(lines[0].bytes, b"one");
        assert_eq!(lines[1].bytes, b"two");
    }

    #[test]
    fn every_line_of_every_entry_is_accounted_for() {
        let (_, lines, entries) = feed(NODE);
        let mut owned = vec![false; lines.len()];
        for e in &entries {
            for i in e.start_line..=e.end_line {
                assert!(!owned[i as usize], "line {i} counted twice");
                owned[i as usize] = true;
            }
        }
        for (i, l) in lines.iter().enumerate() {
            if !vt::render_line(&l.bytes).trim().is_empty() {
                assert!(owned[i], "orphaned line {i}");
            }
        }
    }

    #[test]
    fn a_progress_bar_collapses_to_one_entry() {
        let stream = b"building\r[    ] 0%\r[==  ] 50%\r[====] 100%\ndone\n";
        let (_, lines, entries) = feed(stream);
        assert_eq!(lines.len(), 2, "carriage returns must not create lines");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "[====] 100%");
    }

    #[test]
    fn an_idle_gap_closes_an_open_entry_for_the_live_view() {
        let mut p = Parser::new();
        p.push(b"Error: boom\n", 0);
        let out = p.tick(IDLE_CLOSE_MICROS + 1);
        assert_eq!(out.entries.len(), 1);
        assert!(out.entries[0].is_error);
    }

    #[test]
    fn reparsing_reproduces_the_same_index() {
        let (_, lines, entries) = feed(NODE);
        let (_, again) = reparse(&lines);
        assert_eq!(entries.len(), again.len());
        for (a, b) in entries.iter().zip(again.iter()) {
            assert_eq!(a.hash, b.hash);
            assert_eq!((a.start_line, a.end_line), (b.start_line, b.end_line));
            assert_eq!(a.is_error, b.is_error);
        }
    }

    #[test]
    fn elixir_run_settles_on_colour_bracketing() {
        let mut stream = Vec::new();
        for i in 0..8 {
            stream.extend_from_slice(
                format!("\x1b[32m12:04:1{i}.882 [info] GET /users\x1b[0m\n").as_bytes(),
            );
        }
        stream.extend_from_slice(b"\x1b[31m12:04:20.000 [error] ** (RuntimeError) boom\n    (my_app) lib/a.ex:1: A.b/0\x1b[0m\n");
        let mut p = Parser::new();
        let mut entries = p.push(&stream, 0).entries;
        entries.extend(p.finish(1).entries);
        assert_eq!(p.mode(), DelimiterMode::ColourBracket);
        assert_eq!(entries.len(), 9, "{:#?}", entries.len());
        assert_eq!(entries.iter().filter(|e| e.is_error).count(), 1);
        assert_eq!(entries.last().unwrap().boundaries, Boundaries::Exact);
    }

    #[test]
    fn an_unprefixed_exception_amid_prefixed_logs_is_its_own_error() {
        let mut stream = Vec::new();
        for i in 0..3 {
            for j in 0..20 {
                stream.extend_from_slice(format!("[info] request {i}{j:02} handled in 12ms\n").as_bytes());
            }
            stream.extend_from_slice(
                format!("Error: upstream timeout on shard {i}\n    at fetch (/app/net.js:10:5)\n").as_bytes(),
            );
        }
        let mut p = Parser::new();
        let mut entries = p.push(&stream, 0).entries;
        entries.extend(p.finish(1).entries);
        assert_eq!(p.mode(), DelimiterMode::Prefix);
        let errs: Vec<_> = entries.iter().filter(|e| e.is_error).collect();
        assert_eq!(errs.len(), 3, "{entries:#?}");
        for e in errs {
            assert_eq!(e.level, Level::Error);
            assert!(e.text.starts_with("Error: upstream timeout"), "{:?}", e.text);
            assert!(e.text.contains("at fetch"), "frames stay with their banner: {:?}", e.text);
        }
    }

    #[test]
    fn fallback_mode_admits_that_boundaries_are_inferred() {
        let (_, _, entries) = feed(NODE);
        assert!(entries.iter().all(|e| e.boundaries == Boundaries::Inferred));
    }

    #[test]
    fn identical_errors_share_a_fingerprint_across_entries() {
        let stream = b"Error: connection refused\n    at a (/app/x.js:1:1)\nok\nError: connection refused\n    at a (/app/x.js:9:9)\n";
        let (_, _, entries) = feed(stream);
        let errs: Vec<_> = entries.iter().filter(|e| e.is_error).collect();
        assert_eq!(errs.len(), 2);
        assert_eq!(errs[0].hash, errs[1].hash);
    }
}

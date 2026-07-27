//! Delimiter-mode detection.
//!
//! Modes are tried in order of certainty so that an exact delimiter is preferred over
//! inference, and the winner is recorded on the run so consumers know whether entry
//! boundaries are exact or guessed.
//!
//! Detection samples the opening lines of a run. A mode must earn a clear majority of the
//! sample; otherwise the mode is `Fallback`, whose failure mode is over-splitting.

use crate::model::DelimiterMode;
use regex::Regex;
use std::sync::LazyLock;

/// How many non-blank lines to look at before committing to a mode.
pub const SAMPLE_LINES: usize = 40;
/// Do not commit before this many lines, unless the run ends first.
pub const MIN_SAMPLE: usize = 4;

/// A line that opens a new entry in a timestamp/level-prefixed format.
///
/// Matches only rigid shapes: an ISO or clock timestamp at column zero, a bracketed or
/// bare level token near the start, a logfmt `key=value` level, or a Ruby/Erlang-style
/// single-letter severity. Shapes that require guessing belong in `Fallback`.
pub static LINE_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        ^(?:
            # 2026-07-26T12:04:11 / 2026-07-26 12:04:11
            \d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}
          | # 12:04:11.882  (Elixir, Rails, many others)
            \d{2}:\d{2}:\d{2}(?:[.,]\d{1,6})?\s
          | # [2026-07-26 12:04:11] / [info] / [ERROR]
            \[(?:\d{4}-\d{2}-\d{2}|[A-Za-z]{4,8}\]|[A-Za-z]{4,8}\s)
          | # E, [2026-07-26T12:04:11.882] ERROR -- :   (Ruby Logger)
            [DIWEFU],\s\[
          | # INFO: / ERROR - / WARN  message   (Python logging, Go, java.util.logging)
            (?:TRACE|DEBUG|INFO|NOTICE|WARN(?:ING)?|ERROR|FATAL|CRITICAL|PANIC)\b[\s:\-]
          | # logfmt / structured key=value openers
            (?:level|lvl|severity)=[A-Za-z]+
          | # Elixir/Erlang exception opener at column zero
            \*\*\s\(
          | # Go panic
            panic:\s
          | # npm/yarn/pnpm task banners
            (?:npm|yarn|pnpm|bun)\s(?:ERR!|WARN|notice)
        )",
    )
    .expect("LINE_START regex")
});

/// Scores how well the sample matches per-entry colour bracketing, where a whole entry is
/// wrapped in one SGR pair (`ESC[31m … ESC[0m`) and boundaries are byte-exact.
///
/// Node's `util.inspect` colours individual stack frames with `ESC[39m`, so splitting on
/// any reset would shred one error into many. The score therefore requires the bare
/// `ESC[0m` reset and a colour opening the line, and rejects samples with mid-line resets.
fn colour_bracket_score(raw: &[Vec<u8>]) -> f32 {
    if raw.is_empty() {
        return 0.0;
    }
    let mut opens = 0usize;
    let mut full_resets = 0usize;
    let mut mid_line_resets = 0usize;
    for line in raw {
        if starts_with_sgr(line) {
            opens += 1;
        }
        if let Some(pos) = find(line, b"\x1b[0m") {
            full_resets += 1;
            // A reset followed by more colour is the util.inspect shape: per-token
            // colouring, not per-entry bracketing.
            if pos + 4 < line.len() && find(&line[pos + 4..], b"\x1b[").is_some() {
                mid_line_resets += 1;
            }
        }
        if find(line, b"\x1b[39m").is_some() {
            mid_line_resets += 1;
        }
    }
    let n = raw.len() as f32;
    let open_ratio = opens as f32 / n;
    let reset_ratio = full_resets as f32 / n;
    let noise = mid_line_resets as f32 / n;
    if noise > 0.25 {
        return 0.0;
    }
    open_ratio.min(reset_ratio)
}

fn json_score(text: &[String]) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let hits = text
        .iter()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with('{')
                && serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(t.trim())
                    .is_ok()
        })
        .count();
    hits as f32 / text.len() as f32
}

fn prefix_score(text: &[String]) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    // Continuation lines legitimately fail LINE_START, so excluding them stops a
    // stack-trace-heavy run from demoting an otherwise rigid format.
    let candidates: Vec<&String> = text
        .iter()
        .filter(|l| !super::group::is_continuation(l))
        .collect();
    if candidates.is_empty() {
        return 0.0;
    }
    let hits = candidates.iter().filter(|l| LINE_START.is_match(l)).count();
    hits as f32 / candidates.len() as f32
}

/// Accumulates a sample and decides the mode once.
pub struct Detector {
    raw: Vec<Vec<u8>>,
    text: Vec<String>,
    decided: Option<DelimiterMode>,
}

impl Default for Detector {
    fn default() -> Self {
        Detector {
            raw: Vec::with_capacity(SAMPLE_LINES),
            text: Vec::with_capacity(SAMPLE_LINES),
            decided: None,
        }
    }
}

impl Detector {
    /// Feeds a line, returning the mode once it is settled.
    pub fn observe(&mut self, raw: &[u8], text: &str) -> Option<DelimiterMode> {
        if self.decided.is_some() {
            return self.decided;
        }
        if !text.trim().is_empty() {
            self.raw.push(raw.to_vec());
            self.text.push(text.to_string());
        }
        if self.raw.len() >= SAMPLE_LINES {
            self.decided = Some(self.decide());
        }
        self.decided
    }

    /// Commits to a mode using whatever sample exists.
    ///
    /// Called at end of run, and once enough lines have arrived that waiting longer stops
    /// helping.
    pub fn settle(&mut self) -> DelimiterMode {
        if self.decided.is_none() {
            self.decided = Some(self.decide());
        }
        self.decided.unwrap()
    }

    pub fn decided(&self) -> Option<DelimiterMode> {
        self.decided
    }

    pub fn sample_len(&self) -> usize {
        self.raw.len()
    }

    /// Whether the sample has reached [`MIN_SAMPLE`] lines.
    pub fn has_minimum(&self) -> bool {
        self.raw.len() >= MIN_SAMPLE
    }

    fn decide(&self) -> DelimiterMode {
        if json_score(&self.text) >= 0.8 {
            return DelimiterMode::Json;
        }
        if colour_bracket_score(&self.raw) >= 0.6 {
            return DelimiterMode::ColourBracket;
        }
        if prefix_score(&self.text) >= 0.6 {
            return DelimiterMode::Prefix;
        }
        DelimiterMode::Fallback
    }
}

pub fn starts_with_sgr(line: &[u8]) -> bool {
    if !line.starts_with(b"\x1b[") {
        return false;
    }
    let mut i = 2;
    while i < line.len() && (line[i].is_ascii_digit() || line[i] == b';') {
        i += 1;
    }
    i > 2 && i < line.len() && line[i] == b'm'
}

pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(lines: &[&[u8]]) -> DelimiterMode {
        let mut d = Detector::default();
        for l in lines {
            let text = super::super::vt::render_line(l);
            d.observe(l, &text);
        }
        d.settle()
    }

    #[test]
    fn elixir_colour_bracketing_is_detected() {
        let lines: Vec<&[u8]> = vec![
            b"\x1b[32m12:04:11.882 [info] GET /users\x1b[0m",
            b"\x1b[36m12:04:11.883 [debug] QUERY OK source=\"users\"\x1b[0m",
            b"\x1b[31m12:04:11.884 [error] ** (RuntimeError) boom\x1b[0m",
            b"\x1b[32m12:04:11.900 [info] GET /posts\x1b[0m",
        ];
        assert_eq!(detect(&lines), DelimiterMode::ColourBracket);
    }

    #[test]
    fn node_util_inspect_colouring_is_not_mistaken_for_bracketing() {
        // Per-frame colour with mid-line resets.
        let lines: Vec<&[u8]> = vec![
            b"\x1b[31mError\x1b[39m: \x1b[33mboom\x1b[39m",
            b"    at \x1b[36mObject.<anonymous>\x1b[39m (\x1b[32m/app/s.js:1:1\x1b[39m)",
            b"    at \x1b[36mModule._compile\x1b[39m (\x1b[32mnode:internal:1:1\x1b[39m)",
            b"    at \x1b[36mrequire\x1b[39m (\x1b[32mnode:internal:2:2\x1b[39m)",
        ];
        assert_ne!(detect(&lines), DelimiterMode::ColourBracket);
    }

    #[test]
    fn jsonl_is_detected() {
        let lines: Vec<&[u8]> = vec![
            br#"{"level":"info","msg":"GET /users","ts":1}"#,
            br#"{"level":"error","msg":"boom","ts":2}"#,
            br#"{"level":"info","msg":"GET /posts","ts":3}"#,
            br#"{"level":"debug","msg":"query","ts":4}"#,
        ];
        assert_eq!(detect(&lines), DelimiterMode::Json);
    }

    #[test]
    fn python_logging_is_prefix_mode() {
        let lines: Vec<&[u8]> = vec![
            b"INFO:root:GET /users",
            b"ERROR:root:something failed",
            b"INFO:root:GET /posts",
            b"WARNING:root:slow query",
        ];
        assert_eq!(detect(&lines), DelimiterMode::Prefix);
    }

    #[test]
    fn ruby_logger_is_prefix_mode() {
        let lines: Vec<&[u8]> = vec![
            b"I, [2026-07-26T12:04:11.882000 #123]  INFO -- : GET /users",
            b"E, [2026-07-26T12:04:11.883000 #123] ERROR -- : boom",
            b"I, [2026-07-26T12:04:11.884000 #123]  INFO -- : GET /posts",
            b"D, [2026-07-26T12:04:11.885000 #123] DEBUG -- : cache hit",
        ];
        assert_eq!(detect(&lines), DelimiterMode::Prefix);
    }

    #[test]
    fn bare_node_output_falls_back() {
        // No timestamp, no level, no colour.
        let lines: Vec<&[u8]> = vec![
            b"GET /users",
            b"Error: boom one",
            b"    at Object.<anonymous> (/app/server.js:12:9)",
            b"GET /posts",
        ];
        assert_eq!(detect(&lines), DelimiterMode::Fallback);
    }

    #[test]
    fn stack_frames_do_not_demote_a_rigid_format() {
        let lines: Vec<&[u8]> = vec![
            b"12:04:11.882 [info] GET /users",
            b"12:04:11.884 [error] boom",
            b"    at Object.<anonymous> (/app/server.js:12:9)",
            b"    at Module._compile (node:internal/modules:1:1)",
            b"    at require (node:internal/modules:2:2)",
            b"12:04:11.900 [info] GET /posts",
        ];
        assert_eq!(detect(&lines), DelimiterMode::Prefix);
    }
}

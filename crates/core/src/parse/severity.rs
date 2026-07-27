//! Severity detection.
//!
//! Severity is read from the output, never inferred from its shape. The ladder stops at
//! the first rung that answers:
//!
//! 1. [`Evidence::Declared`] — a severity token, from a JSON field, a logfmt field, or a
//!    keyword in the line's prefix region.
//! 2. [`Evidence::Structural`] — no token, but the runtime printed a banner it emits only
//!    when something threw.
//! 3. [`Evidence::Unknown`] — the producer said nothing. Never promoted to an error.
//!
//! A declared level wins outright: `[debug]` stays debug whatever follows it. This mirrors
//! Loki's detector and OpenTelemetry's rule that an unspecified severity must not be
//! defaulted.

use crate::model::Level;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// How the level was arrived at. Stored per entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Evidence {
    /// A severity token was present and read verbatim.
    Declared,
    /// An exception banner a runtime emits only when something threw.
    Structural,
    /// The producer declared nothing. An absence, not a default.
    Unknown,
}

impl Evidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Evidence::Declared => "declared",
            Evidence::Structural => "structural",
            Evidence::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Evidence {
        match s {
            "declared" => Evidence::Declared,
            "structural" => Evidence::Structural,
            _ => Evidence::Unknown,
        }
    }

    /// Whether this rung may raise a notification. `Unknown` never may.
    pub fn may_notify(self) -> bool {
        matches!(self, Evidence::Declared | Evidence::Structural)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Severity {
    pub level: Level,
    pub evidence: Evidence,
    /// Which rule answered.
    pub signal: Option<&'static str>,
}

impl Severity {
    pub fn unknown() -> Severity {
        Severity {
            level: Level::Unknown,
            evidence: Evidence::Unknown,
            signal: None,
        }
    }

    /// `rung:rule`, e.g. `declared:level_token`.
    pub fn reason(&self) -> Option<String> {
        self.signal.map(|s| format!("{}:{}", self.evidence.as_str(), s))
    }
}

/// A severity token is a prefix field, not prose. Sized to clear the longest real-world
/// prefix: syslog's `Jul 26 12:04:11 host app[1234]:` ends at 37, Ruby's Logger at 35.
const PREFIX_WINDOW: usize = 80;

/// Earliest position in the line wins. List order only breaks ties at the same offset,
/// where the candidates map to the same level anyway (`warn`/`warning`, `err`/`error`).
const LEVEL_WORDS: &[(&str, Level)] = &[
    ("trace", Level::Trace),
    ("verbose", Level::Trace),
    ("debug", Level::Debug),
    ("fatal", Level::Error),
    ("critical", Level::Error),
    ("crit", Level::Error),
    ("panic", Level::Error),
    ("error", Level::Error),
    ("err", Level::Error),
    ("warning", Level::Warn),
    ("warn", Level::Warn),
    ("notice", Level::Info),
    ("info", Level::Info),
];

/// Field names that carry a severity in structured output.
const LEVEL_KEYS: &[&str] = &[
    "level",
    "lvl",
    "severity",
    "severitytext",
    "severity_text",
    "levelname",
    "loglevel",
    "log_level",
];

/// Banners a runtime prints only when something threw.
///
/// Every pattern is anchored at column zero. Loose shapes such as `[A-Z]\w*Error` matching
/// anywhere in a line do not belong here — they fire on prose.
static STRUCTURAL: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let r = |p: &str| Regex::new(p).expect(p);
    vec![
        (r(r"(?m)^\*\* \("), "elixir_exception"),
        (r(r"(?m)^Traceback \(most recent call last\):"), "python_traceback"),
        (
            r(r"(?m)^(?:During handling of the above exception|The above exception was the direct cause)"),
            "python_traceback",
        ),
        (r(r"(?m)^panic: "), "go_panic"),
        (r(r"(?m)^fatal error: "), "go_runtime_fatal"),
        (r(r"(?m)^thread '[^']*' panicked at "), "rust_panic"),
        (r(r"(?m)^Exception in thread "), "java_exception"),
        (r(r"(?m)^Caused by: "), "exception_cause_chain"),
        (r(r"(?m)^Unhandled (?:exception|rejection|promise rejection)"), "unhandled_exception"),
        // Column-zero `SomeError: message` — how Python terminates a traceback and how
        // Node prints an uncaught throw. Anchored and capitalised, so it cannot fire on
        // the word "error" appearing inside a sentence.
        (
            r(r"(?m)^[A-Z][A-Za-z0-9_.]*(?:Error|Exception|Fault|Interrupt):[ \t]"),
            "exception_banner",
        ),
        // Emitted by the kernel or the shell, not by application code.
        (r(r"(?mi)^[^\n]*\bsegmentation fault\b"), "segfault"),
        (r(r"(?mi)^[^\n]*\bcore dumped\b"), "core_dumped"),
    ]
});

/// Detect the severity of one grouped entry.
pub fn detect(text: &str) -> Severity {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");

    if let Some((level, signal)) = declared(first) {
        return Severity {
            level,
            evidence: Evidence::Declared,
            signal: Some(signal),
        };
    }
    if let Some(signal) = structural(text) {
        return Severity {
            level: Level::Error,
            evidence: Evidence::Structural,
            signal: Some(signal),
        };
    }
    Severity::unknown()
}

/// Read a declared severity. Structured fields first: a field named `level` is
/// unambiguous in a way a word in a sentence is not.
fn declared(line: &str) -> Option<(Level, &'static str)> {
    let trimmed = line.trim();

    if looks_json(trimmed) {
        if let Some(level) = json_level(trimmed) {
            return Some((level, "json_field"));
        }
    }
    if let Some(level) = logfmt_level(trimmed) {
        return Some((level, "logfmt_field"));
    }
    // Fall through even when a structured parse found no level field: a JSON line can
    // still carry one inside its message.
    keyword_level(line).map(|l| (l, "level_token"))
}

fn looks_json(s: &str) -> bool {
    s.starts_with('{') && s.ends_with('}')
}

fn json_level(line: &str) -> Option<Level> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = v.as_object()?;
    for (k, val) in obj {
        if !LEVEL_KEYS.contains(&k.to_ascii_lowercase().as_str()) {
            continue;
        }
        if let Some(s) = val.as_str() {
            let level = normalise_level(s);
            if level != Level::Unknown {
                return Some(level);
            }
        }
        if let Some(n) = val.as_i64() {
            let level = from_numeric_level(n);
            if level != Level::Unknown {
                return Some(level);
            }
        }
    }
    None
}

/// `level=error msg="..."` — scan key=value pairs without pulling in a logfmt crate.
fn logfmt_level(line: &str) -> Option<Level> {
    for pair in line.split_whitespace() {
        // Skip non-pairs rather than bailing, or `ts=... level=error` gives up on the
        // first bare word.
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if !LEVEL_KEYS.contains(&k.to_ascii_lowercase().as_str()) {
            continue;
        }
        let level = normalise_level(v.trim_matches('"'));
        if level != Level::Unknown {
            return Some(level);
        }
    }
    None
}

/// Leftmost severity word in the prefix region that reads as a declaration.
///
/// A token boundary alone is not enough: `loaded 12 error handlers` is bounded on both
/// sides and is still prose. See [`is_declaration`] for the extra condition.
fn keyword_level(line: &str) -> Option<Level> {
    let window = &line[..floor_boundary(line, PREFIX_WINDOW)];
    // `to_ascii_lowercase` is length-preserving, so offsets index both alike.
    let lower = window.to_ascii_lowercase();

    let mut best: Option<(usize, Level)> = None;
    for (word, level) in LEVEL_WORDS {
        if let Some(pos) = declared_index(&lower, window, word) {
            if best.is_none_or(|(b, _)| pos < b) {
                best = Some((pos, *level));
            }
            if pos == 0 {
                break;
            }
        }
    }
    best.map(|(_, level)| level)
}

/// Largest index at or below `max` that is a char boundary.
fn floor_boundary(s: &str, max: usize) -> usize {
    let mut i = s.len().min(max);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Leftmost occurrence of `word` that stands alone *and* reads as a declaration.
fn declared_index(lower: &str, original: &str, word: &str) -> Option<usize> {
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(word) {
        let at = from + rel;
        let end = at + word.len();
        let left = if at == 0 { None } else { Some(bytes[at - 1]) };
        let right = bytes.get(end).copied();

        let bounded = left.is_none_or(is_left_boundary) && right.is_none_or(is_right_boundary);
        if bounded && is_declaration(at, left, right, &original[at..end]) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// A colon is not a left boundary: in `misc:error` the word is a value being named.
fn is_left_boundary(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'[' | b'(' | b'{' | b'"' | b'\'' | b'=' | b'|')
}

/// A colon **is** a right boundary, so `debug: message` and `ERROR:root:msg` both read.
fn is_right_boundary(c: u8) -> bool {
    matches!(
        c,
        b' ' | b'\t'
            | b'['
            | b']'
            | b'('
            | b')'
            | b'{'
            | b'}'
            | b':'
            | b','
            | b'!'
            | b'"'
            | b'\''
            | b'='
            | b'|'
    )
}

/// Whether an occurrence reads as a declaration rather than a mention. Any one suffices:
///
/// * position zero — `Error: boom`, `INFO:root:msg`
/// * opened by a delimiter — `[error]`, `(warn)`, `level=error`
/// * closed by a colon — `warning: deprecated`
/// * capitalised — `2026-07-26T12:04:11Z ERROR my_crate: msg`
///
/// Quotes are boundaries but not declarations, which keeps `%{status: "error"}` out. The
/// genuine quoted form `level="error"` is logfmt and never reaches here.
fn is_declaration(at: usize, left: Option<u8>, right: Option<u8>, matched: &str) -> bool {
    at == 0
        || matches!(left, Some(b'[') | Some(b'(') | Some(b'{') | Some(b'='))
        || right == Some(b':')
        || matched.bytes().all(|b| b.is_ascii_uppercase())
}

/// Map a severity word onto our level set. Unrecognised words stay `Unknown` rather than
/// becoming a default, which is the whole point.
pub fn normalise_level(s: &str) -> Level {
    match s.trim().to_ascii_lowercase().as_str() {
        "trace" | "verbose" | "trc" => Level::Trace,
        "debug" | "dbg" | "d" => Level::Debug,
        "info" | "information" | "informational" | "notice" | "inf" | "i" => Level::Info,
        "warn" | "warning" | "wrn" | "w" => Level::Warn,
        "error" | "err" | "fatal" | "critical" | "crit" | "alert" | "emerg" | "emergency"
        | "panic" | "e" | "f" => Level::Error,
        _ => Level::Unknown,
    }
}

/// Numeric `level` fields, on pino and bunyan's scale: 10 trace, 20 debug, 30 info,
/// 40 warn, 50 error, 60 fatal.
///
/// This is *not* OpenTelemetry's 1–24 scale, on which 30 would be an error rather than
/// info. The two disagree over most of their range, and since runlog reads stdout rather
/// than OTLP, a bare numeric `level` in a JSON log line is a Node logger every time.
///
/// Zero and below stay unknown: both scales reserve them for "unspecified".
pub fn from_numeric_level(n: i64) -> Level {
    match n {
        i64::MIN..=0 => Level::Unknown,
        1..=19 => Level::Trace,
        20..=29 => Level::Debug,
        30..=39 => Level::Info,
        40..=49 => Level::Warn,
        _ => Level::Error,
    }
}

fn structural(text: &str) -> Option<&'static str> {
    STRUCTURAL
        .iter()
        .find(|(re, _)| re.is_match(text))
        .map(|(_, name)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Severity {
        detect(s)
    }

    #[test]
    fn a_debug_header_with_indented_detail_is_not_an_error() {
        // Phoenix logs this on every request. Counting the indented lines as stack frames
        // made each one an error.
        let s = d("[debug] Processing with EnlightenWeb.AuthRequestController.keep_alive/2\n  Parameters: %{}\n  Pipelines: [:browser, :authenticated]");
        assert_eq!(s.level, Level::Debug);
        assert_eq!(s.evidence, Evidence::Declared);
    }

    #[test]
    fn a_declared_level_is_never_overridden_by_what_follows_it() {
        // If the traceback belongs to a separate entry, that is a grouping bug. Severity
        // does not compensate by second-guessing the producer.
        let s = d("[debug] rendering\nTraceback (most recent call last):\n  File \"/app/m.py\", line 1");
        assert_eq!(s.level, Level::Debug);
        assert_eq!(s.evidence, Evidence::Declared);
    }

    #[test]
    fn every_common_declaration_shape_reads() {
        for (text, want) in [
            ("12:04:11.882 [info] GET /users", Level::Info),
            ("12:04:11.882 [error] boom", Level::Error),
            ("INFO:root:GET /users", Level::Info),
            ("ERROR:root:boom", Level::Error),
            ("level=warn msg=\"slow query\"", Level::Warn),
            ("ts=2026-07-26 level=error msg=boom", Level::Error),
            (r#"{"level":"warn","msg":"slow"}"#, Level::Warn),
            (r#"{"severity":"ERROR","message":"boom"}"#, Level::Error),
            (r#"{"level":50,"msg":"bad"}"#, Level::Error),
            (r#"{"level":30,"msg":"hi"}"#, Level::Info),
            ("I, [2026-07-26T12:04:11 #1]  INFO -- : GET /users", Level::Info),
            ("E, [2026-07-26T12:04:11 #1] ERROR -- : boom", Level::Error),
            ("2026-07-26T12:04:11.882123Z ERROR my_crate::mod: msg", Level::Error),
            ("npm ERR! code ELIFECYCLE", Level::Error),
            ("WARNING: deprecated flag", Level::Warn),
            ("debug: starting", Level::Debug),
        ] {
            let s = d(text);
            assert_eq!(s.level, want, "{text}");
            assert_eq!(s.evidence, Evidence::Declared, "{text}");
        }
    }

    #[test]
    fn numeric_levels_follow_pinos_scale_not_otels() {
        // 30 is info to pino and error to OpenTelemetry.
        assert_eq!(from_numeric_level(30), Level::Info);
        assert_eq!(from_numeric_level(50), Level::Error);
        // Unspecified stays unknown rather than defaulting to info.
        assert_eq!(from_numeric_level(0), Level::Unknown);
        assert_eq!(d(r#"{"level":0,"msg":"x"}"#).evidence, Evidence::Unknown);
    }

    #[test]
    fn a_level_word_in_the_message_body_does_not_declare_anything() {
        for text in [
            "connecting to the error reporting service",
            "productinformationservice started",
            "GET /api/v1/errors 200",
            "loaded 12 error handlers",
            "starting errorbot",
        ] {
            let s = d(text);
            assert_eq!(
                s.evidence,
                Evidence::Unknown,
                "{text} was read as {:?}",
                s.level
            );
        }
    }

    #[test]
    fn a_level_word_past_the_prefix_region_is_prose() {
        let line = format!("{} error happened", "x".repeat(PREFIX_WINDOW));
        assert_eq!(d(&line).evidence, Evidence::Unknown);
    }

    #[test]
    fn runtime_exception_banners_are_structural() {
        for (text, want_signal) in [
            ("** (Ecto.NoResultsError) expected at least one result", "elixir_exception"),
            ("Traceback (most recent call last):\n  File \"/a.py\", line 1", "python_traceback"),
            ("thread 'main' panicked at src/main.rs:4:5:\nboom", "rust_panic"),
            ("Exception in thread \"main\" java.lang.NullPointerException", "java_exception"),
            ("TypeError: x is not a function", "exception_banner"),
            ("ValueError: boom", "exception_banner"),
        ] {
            let s = d(text);
            assert_eq!(s.level, Level::Error, "{text}");
            assert_eq!(s.evidence, Evidence::Structural, "{text}");
            assert_eq!(s.signal, Some(want_signal), "{text}");
        }
    }

    #[test]
    fn go_banners_declare_their_own_level_at_column_zero() {
        // Level words at position zero, so they resolve as declarations before the
        // structural list is consulted. Either rung is fine; both are errors.
        for text in [
            "panic: runtime error: index out of range [3]\n\tmain.main()",
            "fatal error: concurrent map writes",
        ] {
            let s = d(text);
            assert_eq!(s.level, Level::Error, "{text}");
            assert!(s.evidence.may_notify(), "{text}");
        }
    }

    #[test]
    fn traceback_does_not_read_as_the_trace_level() {
        // `Traceback` opens with the letters of `trace`; only the right-hand boundary
        // check separates a Python crash from trace-level chatter.
        let s = d("Traceback (most recent call last):\n  File \"/a.py\", line 1");
        assert_eq!(s.level, Level::Error);
        assert_eq!(s.signal, Some("python_traceback"));
    }

    #[test]
    fn structural_matching_is_anchored_not_loose() {
        // An unanchored `[A-Z]\w*Error` makes all of these errors.
        for text in [
            "registered handler for ValueError in the retry policy",
            "see docs/RuntimeError.md for details",
            "class UserError < StandardError",
        ] {
            assert_eq!(d(text).evidence, Evidence::Unknown, "{text}");
        }
    }

    #[test]
    fn print_debugging_output_is_unknown_not_an_error() {
        // Not errors, and not info either: the producer said nothing.
        for text in [
            "%{user_id: 42, role: \"admin\"}",
            "HERE: %{conn: #PID<0.123.0>}",
            "[1, 2, 3]",
            ">>> checkpoint reached",
            "{'status': 200}",
            // Clears the boundary check, but a quoted value is not a declaration.
            "%{status: \"error\", retries: 3}",
        ] {
            let s = d(text);
            assert_eq!(s.evidence, Evidence::Unknown, "{text}");
            assert_eq!(s.level, Level::Unknown, "{text}");
        }
    }

    #[test]
    fn indentation_carries_no_severity_whatsoever() {
        let s = d("something happened\n  detail one\n  detail two\n  detail three");
        assert_eq!(s.evidence, Evidence::Unknown);
        assert_eq!(s.level, Level::Unknown);
    }

    #[test]
    fn colon_on_the_left_means_the_word_is_a_value_being_named() {
        assert_eq!(d("component:error handler installed").evidence, Evidence::Unknown);
    }

    #[test]
    fn the_earliest_token_wins_so_the_prefix_beats_the_message() {
        assert_eq!(d("[warn] could not reach the error service").level, Level::Warn);
    }
}

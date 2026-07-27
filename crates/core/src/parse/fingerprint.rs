//! Fingerprinting: collapsing recurring failures into one identity.
//!
//! Dedup, "new vs known", metrics and run diffing all key on the fingerprint rather than
//! on the entry, so a failure retried two hundred times is one identity with a count.
//!
//! Two steps:
//!
//! 1. Pick the identifying line, which is not always the first: every Python traceback
//!    opens with `Traceback (most recent call last):`, so a first-line fingerprint would
//!    collapse `RuntimeError` and `ValueError` into one signature.
//! 2. Normalise it, replacing the parts that vary between occurrences — numbers, hex,
//!    uuids, paths, durations, ids — with placeholders. The resulting template doubles as
//!    the human-readable summary, so there is no second string to keep in sync.

use crate::model::Level;
use regex::Regex;
use std::sync::LazyLock;

pub struct Signature {
    /// Readable, placeholder-substituted line. Shown in the UI and to agents.
    pub template: String,
    /// Stable identity. Changing normalisation changes it; a reparse rebuilds every
    /// fingerprint from the stored lines.
    pub hash: String,
}

/// Rules applied in order. Order matters: paths before numbers, or a path's line number
/// gets replaced first and the path stops matching.
static RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let r = |p: &str| Regex::new(p).expect(p);
    vec![
        // Timestamps first — they contain digits, colons and dashes that later rules eat.
        (r(r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?"), "<ts>"),
        (r(r"\b\d{2}:\d{2}:\d{2}(?:[.,]\d+)?\b"), "<time>"),
        // UUIDs before hex, hex before numbers.
        (r(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b"), "<uuid>"),
        (r(r"\b0x[0-9a-fA-F]+\b"), "<addr>"),
        (r(r"\b[0-9a-fA-F]{32,}\b"), "<hash>"),
        // Paths, with or without a trailing line:col.
        (r(r"(?:[A-Za-z]:)?(?:/|\\)[\w.\-@+]+(?:(?:/|\\)[\w.\-@+]+)+(?::\d+(?::\d+)?)?"), "<path>"),
        (r(r"\b[\w.\-]+\.(?:ex|exs|erl|rb|py|js|ts|tsx|jsx|go|rs|java|kt|php|c|h|cpp|swift):\d+(?::\d+)?"), "<path>"),
        // Network and process identity.
        (r(r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b"), "<ip>"),
        (r(r"\b(?:localhost|127\.0\.0\.1|0\.0\.0\.0):\d+\b"), "<addr>"),
        (r(r"\bpid[= ]\d+\b"), "pid=<n>"),
        (r(r"#PID<\d+\.\d+\.\d+>"), "<pid>"),
        (r(r"#(?:Reference|Port)<[\d.]+>"), "<ref>"),
        // Durations and sizes.
        (r(r"\b\d+(?:\.\d+)?\s?(?:ns|µs|us|ms|s|m|h)\b"), "<dur>"),
        (r(r"\b\d+(?:\.\d+)?\s?(?:B|KB|MB|GB|KiB|MiB|GiB)\b"), "<size>"),
        // Quoted payloads vary per occurrence; their shape does not.
        (r("\x22[^\x22]{0,200}\x22"), "\x22<str>\x22"),
        (r(r"'[^']{0,200}'"), "'<str>'"),
        // Anything numeric that is left.
        (r(r"\b\d+(?:\.\d+)?\b"), "<n>"),
        // Collapse whitespace last so the template reads cleanly.
        (r(r"\s+"), " "),
    ]
});

static PY_TERMINATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_.]*(?:Error|Exception|Warning|Exit|Interrupt|Fault)?:")
        .expect("PY_TERMINATOR")
});

static RUST_PANIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^thread '.*' panicked at ").expect("RUST_PANIC"));

/// Returns the line that identifies this entry.
///
/// Defaults to the first non-blank line, with per-format exceptions:
///
/// * Python — the opener is a constant, so the identity is the column-zero terminator.
/// * Rust — `thread 'main' panicked at src/x.rs:4:5:` is a location; the message follows.
/// * Java / anyhow — `Caused by:` chains put the root cause last, but the top-level
///   exception names the bug, so the first line still wins.
pub fn identifying_line(text: &str) -> &str {
    let lines: Vec<&str> = text.lines().collect();
    let first = lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .copied()
        .unwrap_or("");

    if first.trim_start().starts_with("Traceback (most recent call last):") {
        // The last column-zero `SomeError: message`, not the first, so a chained
        // "during handling of the above exception" reports the outermost cause.
        if let Some(term) = lines
            .iter()
            .rev()
            .find(|l| PY_TERMINATOR.is_match(l) && !l.starts_with(char::is_whitespace))
        {
            return term;
        }
    }

    if RUST_PANIC.is_match(first) {
        if let Some(msg) = lines.iter().skip(1).find(|l| !l.trim().is_empty()) {
            return msg;
        }
    }

    first
}

/// Normalises a line into a readable template.
pub fn normalise(line: &str) -> String {
    let mut s = line.trim().to_string();
    for (re, repl) in RULES.iter() {
        s = re.replace_all(&s, *repl).into_owned();
    }
    // The level is carried separately, so leaving its tag here would make one message at
    // two levels look like two templates.
    let s = strip_level_prefix(&s);
    s.trim().to_string()
}

fn strip_level_prefix(s: &str) -> String {
    static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^(?:<ts>|<time>)?\s*(?:\[(?:trace|debug|info|notice|warn|warning|error|fatal|critical)\]|(?:trace|debug|info|notice|warn|warning|error|fatal|critical)\s*[:\-])\s*",
        )
        .expect("PREFIX")
    });
    PREFIX.replace(s, "").into_owned()
}

/// Fingerprints an entry from its text and level.
pub fn signature(text: &str, level: Level) -> Signature {
    let template = normalise(identifying_line(text));
    let mut hasher = blake3::Hasher::new();
    hasher.update(level.as_str().as_bytes());
    hasher.update(b"\x1f");
    hasher.update(template.as_bytes());
    Signature {
        hash: hasher.finalize().to_hex()[..32].to_string(),
        template,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(text: &str) -> String {
        signature(text, Level::Error).hash
    }

    const PY_RUNTIME: &str = "Traceback (most recent call last):\n  File \"/app/main.py\", line 12, in handler\n    raise RuntimeError(\"boom\")\nRuntimeError: boom";
    const PY_VALUE: &str = "Traceback (most recent call last):\n  File \"/app/main.py\", line 20, in other\n    raise ValueError(\"nope\")\nValueError: nope";

    #[test]
    fn python_tracebacks_do_not_collide_on_their_shared_opener() {
        // Both blocks open with an identical line.
        assert_ne!(sig(PY_RUNTIME), sig(PY_VALUE));
        assert!(identifying_line(PY_RUNTIME).starts_with("RuntimeError"));
        assert!(identifying_line(PY_VALUE).starts_with("ValueError"));
    }

    #[test]
    fn the_same_error_at_different_ids_is_one_signature() {
        let a = "12:04:11.882 [error] GET /users/4821 failed in 12ms";
        let b = "12:09:52.113 [error] GET /users/93 failed in 8ms";
        assert_eq!(sig(a), sig(b));
        assert_eq!(
            normalise(a),
            "GET <path> failed in <dur>",
            "template should stay readable"
        );
    }

    #[test]
    fn different_errors_stay_different() {
        let a = "** (Ecto.NoResultsError) expected at least one result";
        let b = "** (Postgrex.Error) connection not available";
        assert_ne!(sig(a), sig(b));
    }

    #[test]
    fn stack_frame_line_numbers_do_not_split_a_signature() {
        let a = "Error: boom\n    at Object.<anonymous> (/app/server.js:12:9)";
        let b = "Error: boom\n    at Object.<anonymous> (/app/server.js:44:2)";
        assert_eq!(sig(a), sig(b));
    }

    #[test]
    fn rust_panics_identify_on_the_message_not_the_location() {
        let a = "thread 'main' panicked at src/main.rs:4:5:\nindex out of bounds";
        let b = "thread 'tokio-worker' panicked at src/other.rs:99:1:\nindex out of bounds";
        assert_eq!(sig(a), sig(b));
    }

    #[test]
    fn uuids_hashes_and_addresses_are_normalised() {
        let a = "request 3f2b7c1e-4c9a-4f2e-8e1a-9c2b7d4e5f60 failed at 0x7fff1234";
        let b = "request a1b2c3d4-0000-1111-2222-333344445555 failed at 0xdeadbeef";
        assert_eq!(sig(a), sig(b));
        assert_eq!(normalise(a), "request <uuid> failed at <addr>");
    }

    #[test]
    fn the_level_prefix_does_not_change_the_signature() {
        assert_eq!(
            normalise("[error] connection refused"),
            normalise("connection refused")
        );
    }

    #[test]
    fn the_same_message_is_one_signature_across_a_port_change() {
        let a = "Address already in use - bind(2) for 127.0.0.1:4000";
        let b = "Address already in use - bind(2) for 127.0.0.1:4001";
        assert_eq!(sig(a), sig(b));
    }

    #[test]
    fn templates_stay_human_readable() {
        let t = normalise("12:04:11.882 [error] ** (DBConnection.ConnectionError) tcp recv: closed (idle 3200ms)");
        assert!(t.contains("DBConnection.ConnectionError"), "{t}");
        assert!(t.contains("<dur>"), "{t}");
    }

    #[test]
    fn level_participates_in_identity() {
        let text = "connection refused";
        assert_ne!(
            signature(text, Level::Error).hash,
            signature(text, Level::Warn).hash
        );
    }
}

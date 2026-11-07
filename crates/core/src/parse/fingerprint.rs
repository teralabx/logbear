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
    ]
});

/// A value announced as an id, whatever it looks like. The bare form is `req=`, because
/// `req: GET /users` is common and is not an id.
static KEYED_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b((?:(?:x-)?request[_-]?id|req[_-]?id|trace[_-]?id|span[_-]?id|correlation[_-]?id)["']?\s*[=:]\s*["']?|req=["']?)([A-Za-z0-9+/_=.\-]{2,})"#,
    )
    .expect("KEYED_ID")
});

/// Anything long enough to be an unannounced id; [`rewrite_token`] decides whether it is one.
static LONG_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9+_\-]{16,}={0,2}").expect("LONG_TOKEN"));

/// How often neighbouring characters must change between lower, upper and digit for a token
/// to read as random. Near 0.6 for base62, under 0.3 for a camelCase name with a digit in it.
const MIN_CLASS_CHANGE: f64 = 0.45;

/// Replaces a long token if it is an id, which is a question of shape: hex with both digits
/// and letters, or all of upper, lower and digits, changing between them too often for words.
/// Without that last test `handleOAuth2CallbackError` would collapse, and two different
/// failures would share an identity.
fn rewrite_token(token: &str) -> Option<&'static str> {
    let has = |f: fn(&u8) -> bool| token.as_bytes().iter().any(f);
    let (lower, upper, digit) = (
        has(u8::is_ascii_lowercase),
        has(u8::is_ascii_uppercase),
        has(u8::is_ascii_digit),
    );
    if digit && (lower || upper) && token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some("<hash>");
    }
    if !(lower && upper && digit) {
        return None;
    }
    let class = |b: u8| match b {
        b'a'..=b'z' => Some(0),
        b'A'..=b'Z' => Some(1),
        b'0'..=b'9' => Some(2),
        _ => None,
    };
    let (mut pairs, mut changes) = (0u32, 0u32);
    for w in token.as_bytes().windows(2) {
        if let (Some(a), Some(b)) = (class(w[0]), class(w[1])) {
            pairs += 1;
            changes += u32::from(a != b);
        }
    }
    (f64::from(changes) >= MIN_CLASS_CHANGE * f64::from(pairs)).then_some("<id>")
}

/// Collapses the ids in a line.
///
/// A bare request id is the commonest thing in a dev server's log that is unique per line, so
/// each one left in place is a new template, and those tables are kept for good.
fn collapse_ids(s: &str) -> String {
    let keyed = KEYED_ID.replace_all(s, |c: &regex::Captures<'_>| {
        // A count, not an id: the numeric rule below already gives it a better name.
        if c[2].bytes().any(|b| b.is_ascii_alphabetic()) {
            format!("{}<id>", &c[1])
        } else {
            c[0].to_string()
        }
    });
    LONG_TOKEN
        .replace_all(&keyed, |c: &regex::Captures<'_>| {
            rewrite_token(&c[0]).map_or_else(|| c[0].to_string(), str::to_string)
        })
        .into_owned()
}

/// The rules after ids. Separate because they read digits, which would eat into an id.
static AFTER_ID_RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let r = |p: &str| Regex::new(p).expect(p);
    vec![
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
    s = collapse_ids(&s);
    for (re, repl) in AFTER_ID_RULES.iter() {
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

    /// A reproducible stand-in for an id generator.
    fn ids(alphabet: &[u8], len: usize, n: usize) -> Vec<String> {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..n)
            .map(|_| {
                (0..len)
                    .map(|_| alphabet[(next() % alphabet.len() as u64) as usize] as char)
                    .collect()
            })
            .collect()
    }

    const HEX: &[u8] = b"0123456789abcdef";
    const BASE62: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    const BASE64URL: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    fn templates(render: impl Fn(&str) -> String, ids: &[String]) -> usize {
        ids.iter()
            .map(|id| normalise(&render(id)))
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    #[test]
    fn keyed_ids_collapse_whatever_their_alphabet() {
        for key in ["req", "request_id", "requestId", "trace_id", "traceid", "span_id", "correlation_id"] {
            for (alphabet, len) in [(HEX, 16), (HEX, 24), (BASE62, 20), (BASE64URL, 20), (BASE64URL, 12)] {
                let n = templates(|id| format!("GET /users {key}={id} done"), &ids(alphabet, len, 500));
                assert_eq!(n, 1, "{key} over {len} chars of {:?}", &alphabet[..4]);
            }
        }
    }

    #[test]
    fn keyed_ids_read_as_ids() {
        assert_eq!(normalise("failed request_id=igjoNf3kQ9xLm2Pz"), "failed request_id=<id>");
        assert_eq!(normalise("failed req=7ca8d2f01b3e4a96"), "failed req=<id>");
        assert_eq!(normalise("request-id: igjoNf3kQ9xLm2Pz failed"), "request-id: <id> failed");
        assert_eq!(normalise("traceId='igjoNf3kQ9xLm2Pz' failed"), "traceId='<str>' failed");
    }

    #[test]
    fn a_bare_header_style_id_collapses_too() {
        let n = templates(|id| format!("x-request-id: {id} rejected"), &ids(BASE64URL, 22, 500));
        assert_eq!(n, 1);
    }

    #[test]
    fn bare_long_hex_collapses() {
        for len in [16, 20, 24, 31] {
            let n = templates(|id| format!("cache miss for {id} on read"), &ids(HEX, len, 500));
            assert!(n <= 2, "{len} hex chars gave {n} templates");
        }
        assert_eq!(normalise("cache miss for 7ca8d2f01b3e4a96 on read"), "cache miss for <hash> on read");
    }

    #[test]
    fn bare_mixed_case_tokens_collapse() {
        for (alphabet, len) in [(BASE62, 22), (BASE62, 32), (BASE64URL, 22)] {
            let total = 2000;
            let n = templates(|id| format!("session {id} expired"), &ids(alphabet, len, total));
            assert!(n < total / 10, "{n} of {total} ids stayed distinct");
        }
    }

    #[test]
    fn a_stream_of_unique_request_ids_is_one_signature_not_one_each() {
        let mut seen = std::collections::HashSet::new();
        for (req, trace) in ids(BASE64URL, 20, 5000).iter().zip(ids(HEX, 32, 5000)) {
            let line = format!("[error] request_id={req} trace_id={trace} upstream failed");
            seen.insert(sig(&line));
        }
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn ordinary_words_and_identifiers_survive() {
        for word in [
            "handleOAuth2CallbackError",
            "sha256WithRSAEncryption2048",
            "ERR_HTTP2_STREAM_ERROR",
            "supercalifragilisticexpialidocious",
            "getUserProfileByEmailAddress",
            "XMLHttpRequestEventTarget",
            "AbstractSingletonProxyFactoryBean",
            "x86_64-unknown-linux-gnu",
            "my-really-long-service-name-v2",
            "Base64EncoderInputStream",
            "deadbeefdeadbeefdeadbeef",
        ] {
            let line = format!("failed in {word} while loading");
            assert_eq!(normalise(&line), line, "{word} was collapsed");
        }
    }

    #[test]
    fn templates_that_had_no_ids_in_them_do_not_move() {
        for (line, template) in [
            ("12:04:11.882 [error] GET /users/4821 failed in 12ms", "GET <path> failed in <dur>"),
            ("request 3f2b7c1e-4c9a-4f2e-8e1a-9c2b7d4e5f60 failed at 0x7fff1234", "request <uuid> failed at <addr>"),
            ("req=42 failed", "req=<n> failed"),
            ("request_id=3f2b7c1e-4c9a-4f2e-8e1a-9c2b7d4e5f60", "request_id=<uuid>"),
            ("req: GET /users/4821", "req: GET <path>"),
            ("req=/users/4821/edit failed", "req=<path> failed"),
            ("loaded /app/Abc123Def456Ghi789Jkl/file.js", "loaded <path>"),
            ("value 1234567890123456789 out of range", "value <n> out of range"),
            ("Address already in use - bind(2) for 127.0.0.1:4000", "Address already in use - bind(<n>) for <ip>"),
            ("** (Ecto.NoResultsError) expected at least one result", "** (Ecto.NoResultsError) expected at least one result"),
        ] {
            assert_eq!(normalise(line), template, "{line}");
        }
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

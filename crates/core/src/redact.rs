//! Secret redaction, applied before the write.
//!
//! Redacting on ingest rather than on read is load-bearing: a secret that never lands
//! cannot leak through an export, a sync, a query bug, or an agent reading a log.
//!
//! Masking preserves byte length, because `chunks` must replay byte-exact into a terminal
//! and a shorter marker would shift everything after it. Escape sequences survive too: the
//! patterns run over the text a terminal would show, so a colour code can neither hide a
//! secret from them nor be masked along with one.

use regex::bytes::{Regex, RegexSet};
use std::sync::{LazyLock, OnceLock};

/// Patterns matched against the visible bytes of a line, ordered roughly by confidence.
const PATTERNS: &[&str] = &[
    // AWS access key id / secret pairs
    r"(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b",
    r"(?i)\baws_secret_access_key\s*[=:]\s*\S+",
    // GitHub, GitLab, Slack, Stripe, OpenAI, Anthropic token shapes
    r"gh[pousr]_[A-Za-z0-9]{16,}\b",
    r"github_pat_[A-Za-z0-9_]{20,}\b",
    r"glpat-[A-Za-z0-9_\-]{16,}\b",
    r"xox[abposr]-[A-Za-z0-9\-]{10,}\b",
    r"xapp-[A-Za-z0-9\-]{10,}\b",
    r"\b[rs]k_(?:live|test)_[A-Za-z0-9]{16,}\b",
    r"\bsk-(?:proj-|ant-)?[A-Za-z0-9_\-]{20,}\b",
    // Google, npm, Hugging Face
    r"\bAIza[0-9A-Za-z_\-]{30,}",
    r"\bnpm_[A-Za-z0-9]{20,}\b",
    r"\bhf_[A-Za-z0-9]{20,}\b",
    // JWTs
    r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\b",
    // Authorization headers
    r"(?i)\b(?:authorization|proxy-authorization)\s*[=:]\s*(?:bearer|basic|token)\s+\S+",
    r"(?i)\bbearer\s+[A-Za-z0-9._\-]{20,}",
    r"(?i)\b(?:x-)?api-key\s*[=:]\s*\S+",
    // Cookies run to the end of the line: every pair in them may be a session.
    r"(?i)\b(?:set-)?cookie\s*:\s*(?-u:[^\r\n])+",
    // Connection strings carrying credentials, with or without a user name
    r"(?i)\b[a-z][a-z0-9+.\-]*://[^\s:@/]*:[^\s@/]+@",
    // JSON and JS object keys that announce a secret, escaped or not. String values only:
    // `"input_tokens": 1500` is a count, and masking counts would gut ordinary logs.
    r#"(?i)\\?["'][\w\-]*(?:secret|token|password|passwd|pwd|passphrase|api[_\-]?key|private[_\-]?key|access[_\-]?key|credential|cookie)[\w\-]*\\?["']\s*:\s*(?:"(?:[^"\\]|\\.)*"|\\"(?:[^"\\]|\\[^"])*\\"|'[^']*')"#,
    r#"(?i)\\?["'](?:auth|authorization)\\?["']\s*:\s*(?:"(?:[^"\\]|\\.)*"|\\"(?:[^"\\]|\\[^"])*\\"|'[^']*')"#,
    // KEY=value where the key name announces itself
    r"(?i)\b[A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|APIKEY|API_KEY|PRIVATE_KEY|CREDENTIAL|ACCESS_KEY|AUTH)[A-Z0-9_]*\s*[=:]\s*(?:\x22[^\x22]*\x22|'[^']*'|[^\s;,]+)",
    // A bare `PASS:` is a test runner's verdict, so the short forms need a qualifier.
    r"(?i)\b[A-Z0-9_]*_(?:PASS|PWD)\s*[=:]\s*(?:\x22[^\x22]*\x22|'[^']*'|[^\s;,]+)",
];

/// A block's first and last lines. Handled by [`pem_spans`] rather than listed in
/// [`PATTERNS`], because capture sees one line at a time and a key spans dozens.
static PEM_BEGIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----").expect("PEM_BEGIN")
});
static PEM_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----").expect("PEM_END")
});

/// How many lines a block may mask without its END arriving. A 4096-bit key is about 50, and
/// a stray BEGIN must not blank the rest of the run.
const MAX_PEM_LINES: usize = 200;

/// Replacement glyph. U+2022 is three bytes in UTF-8, so masks are sized by byte budget
/// rather than character count.
const DOT: &[u8] = "•".as_bytes();

/// Stands in for a flag's value in argv, where nothing has to replay at the original length
/// and a fixed width says nothing about the password.
const ARGV_MASK: &str = "••••••••";

/// A compiled pattern list. The set is the prefilter that keeps the ingest hot path
/// affordable: a line with no secret costs one pass and no per-regex work.
struct PatternSet {
    set: RegexSet,
    regexes: Vec<Regex>,
}

impl PatternSet {
    fn empty() -> PatternSet {
        PatternSet {
            set: RegexSet::empty(),
            regexes: Vec::new(),
        }
    }

    fn collect_spans(&self, text: &[u8], spans: &mut Vec<(usize, usize)>) {
        for i in self.set.matches(text) {
            for m in self.regexes[i].find_iter(text) {
                if !m.is_empty() {
                    spans.push((m.start(), m.end()));
                }
            }
        }
    }
}

/// Compiled once and kept apart from user patterns, so nothing a user configures can take
/// them down. A test compiles them; a bad edit fails there rather than in the field.
static BUILT_IN: LazyLock<PatternSet> = LazyLock::new(|| PatternSet {
    set: RegexSet::new(PATTERNS).expect("built-in redaction patterns"),
    regexes: PATTERNS
        .iter()
        .map(|p| Regex::new(p).expect("built-in redaction pattern"))
        .collect(),
});

/// What redaction must remember between lines of one stream.
#[derive(Debug, Default, Clone)]
pub struct LineState {
    /// Lines a private key block may still mask; zero outside a block.
    pem_lines_left: usize,
}

pub struct Redactor {
    user: PatternSet,
    rejected: Vec<String>,
    enabled: bool,
}

impl Redactor {
    pub fn new(enabled: bool, extra: &[String]) -> Redactor {
        let (user, rejected) = compile_user_patterns(extra);
        // Evaluated here so a daemon that cannot mask fails at start, not on some later line.
        LazyLock::force(&BUILT_IN);
        Redactor {
            user,
            rejected,
            enabled,
        }
    }

    pub fn disabled() -> Redactor {
        Redactor {
            user: PatternSet::empty(),
            rejected: Vec::new(),
            enabled: false,
        }
    }

    /// User patterns that were dropped, so settings can say which ones are not in force.
    pub fn rejected(&self) -> &[String] {
        &self.rejected
    }

    /// Redacts one self-contained buffer in place, returning the number of secrets masked.
    pub fn redact(&self, buf: &mut [u8]) -> usize {
        self.redact_line(buf, &mut LineState::default())
    }

    /// Redacts the next line of a stream, carrying what spans lines in `state`.
    pub fn redact_line(&self, buf: &mut [u8], state: &mut LineState) -> usize {
        if !self.enabled || buf.is_empty() {
            return 0;
        }
        let view = Visible::of(buf);
        let text: &[u8] = view.as_ref().map_or(&*buf, |v| &v.text);

        let mut spans = self.spans(text);
        pem_spans(text, state, &mut spans);
        let spans = merge(spans);
        let masked = spans.len();

        match view {
            None => spans.iter().for_each(|&(s, e)| mask(buf, s, e)),
            Some(view) => {
                view.mask(buf, &spans);
                // A hyperlink's target is not shown, but it is stored and may hold a token.
                for r in view.payloads {
                    for (s, e) in merge(self.spans(&buf[r.clone()])) {
                        mask(buf, r.start + s, r.start + e);
                    }
                }
            }
        }
        masked
    }

    fn spans(&self, text: &[u8]) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        BUILT_IN.collect_spans(text, &mut spans);
        self.user.collect_spans(text, &mut spans);
        spans
    }

    /// Convenience wrapper for text paths such as settings echoes and notification bodies.
    pub fn redact_str(&self, s: &str) -> String {
        let mut buf = s.as_bytes().to_vec();
        self.redact(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Masks the secrets in a command line, which log patterns alone cannot see: in argv the
    /// flag and its value are separate words, and whether `-p` is a password depends on the
    /// program.
    ///
    /// Flag names stay readable and only their values go, so a run is still recognisable.
    pub fn redact_argv(&self, argv: &[String]) -> Vec<String> {
        if !self.enabled {
            return argv.to_vec();
        }
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.redact_words(&words, true)
    }

    fn redact_words(&self, words: &[&str], split_shell_strings: bool) -> Vec<String> {
        let mut out = Vec::with_capacity(words.len());
        let mut programs: Vec<&str> = Vec::new();
        let mut next = NextWord::Plain;

        for word in words {
            let pending = std::mem::replace(&mut next, NextWord::Plain);
            if pending.hides(word) {
                out.push(ARGV_MASK.to_string());
                continue;
            }
            if !word.starts_with('-') {
                programs.push(word.rsplit('/').next().unwrap_or(word));
            }
            match classify_flag(word, &programs) {
                FlagValue::Attached(at) => out.push(format!("{}{ARGV_MASK}", &word[..at])),
                FlagValue::Follows(kind) => {
                    next = kind;
                    out.push(word.to_string());
                }
                FlagValue::None if split_shell_strings && word.contains(' ') => {
                    // `sh -c "mysql --password x"` is one word holding a whole command.
                    let inner: Vec<&str> = word.split(' ').collect();
                    out.push(self.redact_str(&self.redact_words(&inner, false).join(" ")));
                }
                FlagValue::None => out.push(self.redact_str(word)),
            }
        }
        out
    }
}

/// Compiles user patterns, dropping and logging each one that cannot be used.
///
/// The set is rebuilt with each addition because a pattern can be fine alone and still push
/// the combined set past its size limit; only the one that does so is dropped.
fn compile_user_patterns(extra: &[String]) -> (PatternSet, Vec<String>) {
    let mut compiled = PatternSet::empty();
    let mut accepted: Vec<&str> = Vec::new();
    let mut rejected = Vec::new();

    for p in extra {
        let tried = Regex::new(p).map_err(|e| e.to_string()).and_then(|re| {
            accepted.push(p);
            let set = RegexSet::new(&accepted).map_err(|e| {
                accepted.pop();
                e.to_string()
            })?;
            Ok((re, set))
        });
        match tried {
            Ok((re, set)) => {
                compiled.regexes.push(re);
                compiled.set = set;
            }
            Err(e) => {
                // Ends up in logbeard.log; capture carries on either way.
                eprintln!("logbear: dropped redaction pattern {p:?}: {e}");
                rejected.push(p.clone());
            }
        }
    }
    (compiled, rejected)
}

/// Adds the spans of private key material in this line, tracking blocks across lines.
fn pem_spans(text: &[u8], state: &mut LineState, spans: &mut Vec<(usize, usize)>) {
    let mut pos = 0;
    loop {
        if state.pem_lines_left == 0 {
            match PEM_BEGIN.find_at(text, pos) {
                Some(m) => {
                    state.pem_lines_left = MAX_PEM_LINES;
                    pos = m.start();
                }
                None => return,
            }
        }
        match PEM_END.find_at(text, pos) {
            Some(m) => {
                spans.push((pos, m.end()));
                state.pem_lines_left = 0;
                pos = m.end();
            }
            None => {
                spans.push((pos, text.len()));
                state.pem_lines_left -= 1;
                return;
            }
        }
    }
}

/// Sorts spans and joins the overlapping ones, so nested matches are masked once.
fn merge(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.retain(|(s, e)| s < e);
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

fn mask(buf: &mut [u8], start: usize, end: usize) {
    buf[start..end].copy_from_slice(&mask_of(end - start));
}

fn mask_of(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() + DOT.len() <= len {
        out.extend_from_slice(DOT);
    }
    // Pad the remainder so the byte length matches exactly.
    while out.len() < len {
        out.push(b'.');
    }
    out
}

/// A line with its escape sequences lifted out.
///
/// `\b` fails between the `m` of `ESC[33m` and a token, because both are word characters, and
/// a value can be broken up by colour changes. Matching this projection answers both, and
/// mapping a match back masks only the bytes either side of each sequence.
struct Visible {
    text: Vec<u8>,
    /// Consecutive stretches of `text`, as (start in `text`, start in the line, length).
    runs: Vec<(usize, usize, usize)>,
    /// Bodies of OSC, DCS and similar strings, in line offsets. Not visible, so not in
    /// `text`, but scanned on their own.
    payloads: Vec<std::ops::Range<usize>>,
}

impl Visible {
    /// `None` for a line with no escapes, the common case, which is matched in place.
    fn of(buf: &[u8]) -> Option<Visible> {
        let first = buf.iter().position(|&b| b == 0x1b)?;
        let mut v = Visible {
            text: Vec::with_capacity(buf.len()),
            runs: Vec::new(),
            payloads: Vec::new(),
        };
        v.push_run(buf, 0, first);
        let mut i = first;
        while i < buf.len() {
            if buf[i] == 0x1b {
                i = v.skip_escape(buf, i);
            } else {
                let end = buf[i..]
                    .iter()
                    .position(|&b| b == 0x1b)
                    .map_or(buf.len(), |n| i + n);
                v.push_run(buf, i, end);
                i = end;
            }
        }
        Some(v)
    }

    fn push_run(&mut self, buf: &[u8], start: usize, end: usize) {
        if start < end {
            self.runs.push((self.text.len(), start, end - start));
            self.text.extend_from_slice(&buf[start..end]);
        }
    }

    /// Returns the offset just past the sequence at `esc`.
    fn skip_escape(&mut self, buf: &[u8], esc: usize) -> usize {
        let at = |i: usize| buf.get(i).copied();
        match at(esc + 1) {
            Some(b'[') => {
                let mut i = esc + 2;
                while at(i).is_some_and(|b| (0x20..=0x3f).contains(&b)) {
                    i += 1;
                }
                i + usize::from(at(i).is_some_and(|b| (0x40..=0x7e).contains(&b)))
            }
            Some(b']' | b'P' | b'X' | b'^' | b'_') => {
                let start = esc + 2;
                let end = buf[start..]
                    .iter()
                    .position(|&b| b == 0x07 || b == 0x1b)
                    .map_or(buf.len(), |n| start + n);
                self.payloads.push(start..end);
                match (at(end), at(end + 1)) {
                    (Some(0x07), _) => end + 1,
                    (Some(0x1b), Some(b'\\')) => end + 2,
                    _ => end,
                }
            }
            Some(0x20..=0x2f) => {
                let mut i = esc + 2;
                while at(i).is_some_and(|b| (0x20..=0x2f).contains(&b)) {
                    i += 1;
                }
                i + usize::from(at(i).is_some())
            }
            Some(_) => esc + 2,
            None => esc + 1,
        }
    }

    /// Masks the line bytes behind sorted, disjoint spans of `text`, stepping over escapes.
    fn mask(&self, buf: &mut [u8], spans: &[(usize, usize)]) {
        let mut first = 0;
        for &(s, e) in spans {
            while first < self.runs.len() && self.runs[first].0 + self.runs[first].2 <= s {
                first += 1;
            }
            for &(at, orig, len) in self.runs[first..].iter().take_while(|r| r.0 < e) {
                let from = s.max(at) - at;
                let to = e.min(at + len) - at;
                mask(buf, orig + from, orig + to);
            }
        }
    }
}

/// What the word after a flag is.
enum NextWord {
    Plain,
    /// A flag that takes a secret, so whatever follows is one unless it is another flag.
    Secret,
    /// `-p` on a program of unknown habits: a password unless the value says otherwise.
    Unless,
}

impl NextWord {
    fn hides(&self, word: &str) -> bool {
        match self {
            NextWord::Plain => false,
            NextWord::Secret => !word.starts_with("--"),
            NextWord::Unless => !is_plainly_not_a_password(word),
        }
    }
}

enum FlagValue {
    None,
    /// In the same word, starting at this offset.
    Attached(usize),
    /// In the next word.
    Follows(NextWord),
}

/// `-p` is a password to these, attached or not.
const PASSWORD_P: &[&str] = &["mysql", "mariadb", "mysqldump", "mysqladmin", "sshpass"];

/// And to these, only as a word of its own. `login` is `docker login` and its relatives.
const DETACHED_PASSWORD_P: &[&str] = &["mongo", "mongosh", "mongodump", "login"];

/// Programs whose `-p` is something else: a package, a port, a boolean, an expression.
///
/// Unknown programs get masked, so this list costs a mangled display when it is short, never
/// a leak. The exception is that [`PASSWORD_P`] wins, for `docker exec db mysql -p x`.
const P_NOT_A_PASSWORD: &[&str] = &[
    "bun", "cargo", "cp", "deno", "docker", "docker-compose", "go", "helm", "hugo", "kubectl",
    "ls", "lsof", "mkdir", "mv", "nc", "netstat", "next", "node", "npx", "perl", "pg_dump",
    "pnpx", "podman", "ps", "psql", "pytest", "python", "python3", "rails", "read", "rsync",
    "ruby", "scp", "sed", "ssh", "tar", "time", "tsc", "uv", "uvx", "vite",
];

/// Flags that name a credential only on particular programs, as (program, flag).
const PROGRAM_FLAGS: &[(&str, &str)] = &[
    ("redis-cli", "-a"),
    ("curl", "-u"),
    ("curl", "--user"),
    ("wget", "--user"),
    ("zip", "-P"),
    ("unzip", "-P"),
    ("sqlcmd", "-P"),
    ("mosquitto_pub", "-P"),
    ("mosquitto_sub", "-P"),
];

fn classify_flag(word: &str, programs: &[&str]) -> FlagValue {
    if !word.starts_with('-') || word == "-" || word == "--" {
        return FlagValue::None;
    }
    let (flag, attached) = match word.find('=') {
        Some(at) => (&word[..at], Some(at + 1)),
        None => (word, None),
    };
    let on = |list: &[&str]| programs.iter().any(|p| list.contains(p));

    let takes_secret = is_secret_flag_name(flag.trim_start_matches('-'))
        || PROGRAM_FLAGS
            .iter()
            .any(|(program, f)| *f == flag && programs.contains(program));
    if takes_secret {
        return attached.map_or(FlagValue::Follows(NextWord::Secret), FlagValue::Attached);
    }

    if word == "-p" {
        return if on(PASSWORD_P) || on(DETACHED_PASSWORD_P) {
            FlagValue::Follows(NextWord::Secret)
        } else if on(P_NOT_A_PASSWORD) {
            FlagValue::None
        } else {
            FlagValue::Follows(NextWord::Unless)
        };
    }
    if word.len() > 2 && word.starts_with("-p") && !word.starts_with("--") && on(PASSWORD_P) {
        return FlagValue::Attached(2);
    }
    FlagValue::None
}

/// Whether a long flag's own name says it carries a credential.
///
/// Compared by word, because `--author` is not `auth` and `--bypass` is not `pass`.
fn is_secret_flag_name(name: &str) -> bool {
    if name.len() < 2 {
        return false;
    }
    let name = name.to_ascii_lowercase().replace('-', "_");
    let words: Vec<&str> = name.split('_').collect();
    let last = words[words.len() - 1];

    // These name somewhere the secret lives, or something about it, rather than the secret.
    const NOT_THE_VALUE: &[&str] = &[
        "cmd", "command", "dir", "env", "expiry", "file", "helper", "jar", "length", "path",
        "prompt", "stdin", "ttl", "type", "url",
    ];
    if words[0] == "no" || NOT_THE_VALUE.contains(&last) {
        return false;
    }
    const SECRET_WORDS: &[&str] = &[
        "apikey", "auth", "authorization", "cookie", "credential", "credentials", "passphrase",
        "passwd", "password", "secret", "token",
    ];
    words.iter().any(|w| SECRET_WORDS.contains(w))
        || matches!(last, "pass" | "pwd")
        || words
            .windows(2)
            .any(|w| matches!(w, ["api" | "private" | "access", "key"]))
}

/// A value that is evidently a port, a version, a path or another flag.
fn is_plainly_not_a_password(word: &str) -> bool {
    word.starts_with(['-', '.', '~'])
        || word.contains('/')
        || word.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ':')
}

/// Process-wide default redactor, for callers with no settings at hand.
pub fn default_redactor() -> &'static Redactor {
    static R: OnceLock<Redactor> = OnceLock::new();
    R.get_or_init(|| Redactor::new(true, &[]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red(s: &str) -> String {
        default_redactor().redact_str(s)
    }

    #[test]
    fn masks_common_token_shapes() {
        for input in [
            "key AKIAIOSFODNN7EXAMPLE here",
            "token ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456",
            "DATABASE_URL=postgres://admin:s3cr3t@db.internal:5432/app",
            "export API_SECRET=supersecretvalue",
            "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            let out = red(input);
            assert!(out.contains('•'), "nothing masked in: {input} -> {out}");
        }
    }

    #[test]
    fn length_is_preserved_so_replay_stays_aligned() {
        let input = "prefix ghp_abcdefghijklmnopqrstuvwxyz0123456789 suffix";
        let out = red(input);
        assert_eq!(out.len(), input.len(), "byte length drifted: {out}");
        assert!(out.starts_with("prefix "));
        assert!(out.ends_with(" suffix"));
    }

    #[test]
    fn ordinary_log_lines_are_untouched() {
        for input in [
            "12:04:11.882 [info] GET /users",
            "** (Ecto.NoResultsError) expected at least one result",
            "    at Object.<anonymous> (/app/server.js:12:9)",
            "compiled 41 files in 2.3s",
        ] {
            assert_eq!(red(input), input, "false positive on: {input}");
        }
    }

    #[test]
    fn pem_blocks_go_whole() {
        let input =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEow==\n-----END RSA PRIVATE KEY-----";
        let out = red(input);
        assert!(!out.contains("MIIEow"), "key body survived: {out}");
    }

    #[test]
    fn a_bad_user_pattern_does_not_break_capture() {
        let r = Redactor::new(true, &["([unclosed".to_string()]);
        let mut buf = b"plain line".to_vec();
        assert_eq!(r.redact(&mut buf), 0);
        assert_eq!(buf, b"plain line");
    }

    /// Each compiles alone, but together they overflow what a `RegexSet` will accept, which
    /// is the failure that used to leave the redactor with an empty set.
    fn too_big_together() -> Vec<String> {
        (0..8).map(|i| format!(r"(?-u:\w){{{}}}x{i}", 40_000 + i)).collect()
    }

    #[test]
    fn the_overflow_fixture_really_does_overflow() {
        for p in too_big_together() {
            assert!(Regex::new(&p).is_ok(), "{p} should compile alone");
        }
        assert!(
            RegexSet::new(too_big_together()).is_err(),
            "the fixture no longer overflows a RegexSet"
        );
    }

    #[test]
    fn user_patterns_that_overflow_never_disable_the_built_ins() {
        let r = Redactor::new(true, &too_big_together());
        assert!(r.redact_str("token ghp_abcdefghijklmnopqrstuvwxyz0123456789").contains('•'));
    }

    #[test]
    fn a_rejected_user_pattern_takes_only_itself_with_it() {
        let mut extra = vec!["internal-[0-9]{6}".to_string(), "([unclosed".to_string()];
        extra.extend(too_big_together());
        extra.push("second-[0-9]{6}".to_string());
        let r = Redactor::new(true, &extra);

        assert!(r.rejected().iter().any(|p| p == "([unclosed"), "{:?}", r.rejected());
        assert!(r.rejected().len() > 1, "the overflow should be rejected too");
        for line in [
            "id internal-123456",
            "id second-123456",
            "key AKIAIOSFODNN7EXAMPLE",
        ] {
            assert!(r.redact_str(line).contains('•'), "unmasked: {line}");
        }
    }

    #[test]
    fn argv_flags_carrying_secrets_are_masked() {
        let r = default_redactor();
        let cases: &[(&[&str], &str)] = &[
            (&["app", "--password=hunter2"], "hunter2"),
            (&["app", "--password", "hunter2"], "hunter2"),
            (&["app", "--token", "hunter2"], "hunter2"),
            (&["app", "--api-key=hunter2"], "hunter2"),
            (&["app", "--client_secret", "hunter2"], "hunter2"),
            (&["app", "-password=hunter2"], "hunter2"),
            (&["app", "--db-pass", "hunter2"], "hunter2"),
            (&["app", "-p", "hunter2"], "hunter2"),
            (&["mysql", "-uroot", "-phunter2", "app"], "hunter2"),
            (&["docker", "exec", "db", "mysql", "-phunter2"], "hunter2"),
            (&["sshpass", "-p", "hunter2", "ssh", "host"], "hunter2"),
            (&["docker", "login", "-p", "hunter2"], "hunter2"),
            (&["redis-cli", "-a", "hunter2", "ping"], "hunter2"),
            (&["curl", "-u", "me:hunter2", "http://x"], "hunter2"),
            (&["curl", "--user", "me:hunter2", "http://x"], "hunter2"),
            (&["curl", "-H", "Authorization: Bearer abcdefghijklmnopqrstuvwxyz1"], "abcdefgh"),
            (&["app", "--url=postgres://me:hunter2@db/x"], "hunter2"),
            (&["sh", "-c", "mysql --password hunter2 -e 'select 1'"], "hunter2"),
            (&["sh", "-c", "API_TOKEN=hunter2 ./run"], "hunter2"),
            (&["env", "DB_PASS=hunter2", "./run"], "hunter2"),
        ];
        for (argv, secret) in cases {
            let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let out = r.redact_argv(&argv).join(" ");
            assert!(!out.contains(secret), "{argv:?} -> {out}");
            assert!(out.contains('•'), "{argv:?} -> {out}");
        }
    }

    /// A shell command in argv spells its colours out, so no escape is there to lift away and
    /// the `m` of a literal `\033[33m` touches the token. The prefixes are distinctive enough
    /// to match without a word boundary in front of them.
    #[test]
    fn a_literal_colour_code_cannot_shield_a_token() {
        for token in [
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "github_pat_11ABCDEFG0abcdefghijkl_abcdefghij",
            "glpat-abcdefghijklmnop1234",
            "xoxb-1234567890-abcdefghij",
            "xapp-1-A0123456789-1234567890",
        ] {
            let argv = vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("printf '\\033[33m{token}\\033[0m'"),
            ];
            let out = default_redactor().redact_argv(&argv).join(" ");
            assert!(!out.contains(token), "{out}");
        }
    }

    #[test]
    fn argv_names_stay_readable_when_their_value_goes() {
        let argv: Vec<String> = ["mix", "--password=hunter2", "--token", "hunter2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = default_redactor().redact_argv(&argv);
        assert_eq!(out[0], "mix");
        assert!(out[1].starts_with("--password="), "{out:?}");
        assert_eq!(out[2], "--token");
    }

    #[test]
    fn ordinary_argv_is_left_alone() {
        for argv in [
            vec!["mix", "phx.server"],
            vec!["npm", "run", "dev", "--", "--port", "3000"],
            vec!["next", "dev", "-p", "3000"],
            vec!["docker", "run", "-p", "8080:80", "nginx"],
            vec!["cargo", "test", "-p", "logbear-core"],
            vec!["mkdir", "-p", "tmp/cache"],
            vec!["cp", "-p", "a", "b"],
            vec!["npx", "tsc", "-p", "tsconfig.json"],
            vec!["node", "-p", "process.version"],
            vec!["psql", "-h", "db", "-p", "5432"],
            vec!["git", "commit", "--author", "someone"],
            vec!["docker", "login", "--password-stdin"],
            vec!["app", "--token-file", "/run/token"],
            vec!["app", "--max-tokens", "100"],
            vec!["app", "--no-auth", "serve"],
            vec!["app", "--token", "--verbose"],
        ] {
            let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            assert_eq!(default_redactor().redact_argv(&argv), argv);
        }
    }

    #[test]
    fn disabled_redaction_leaves_argv_alone() {
        let argv = vec!["app".to_string(), "--password=hunter2".to_string()];
        assert_eq!(Redactor::disabled().redact_argv(&argv), argv);
    }
}

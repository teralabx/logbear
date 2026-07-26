//! Level assignment and error detection.
//!
//! Error detection is a **union of independent signals**, deliberately biased to
//! over-catch, and it does not depend on grouping being right. Any signal firing is
//! enough. The failure mode we accept is flagging something that was not an error; the
//! failure mode we refuse is silently missing one, because the entire product is "tell the
//! agent something broke".
//!
//! Which signals fired is recorded on the entry, so a noisy rule can be found and tuned
//! against real data instead of argued about.

use crate::model::Level;
use regex::Regex;
use std::sync::LazyLock;

static LEVEL_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?xi)
        (?:
            \[\s*(?P<a>trace|debug|info|notice|warn|warning|error|err|fatal|critical|crit|alert|emerg)\s*\]
          | \b(?:level|lvl|severity)\s*[=:]\s*\x22?(?P<b>trace|debug|info|notice|warn|warning|error|err|fatal|critical)\x22?
          | ^\s*(?P<c>trace|debug|info|notice|warn|warning|error|fatal|critical|panic)\b\s*[:\-\x20]
          | ^[\x20]*(?P<d>[DIWEF]),\x20\[            # Ruby Logger severity letter
          | \b(?P<e>TRACE|DEBUG|INFO|NOTICE|WARN|WARNING|ERROR|FATAL|CRITICAL)\b\s+--
        )",
    )
    .expect("LEVEL_TAG regex")
});

/// Shapes that are an error regardless of whether anything tagged them one.
static EXCEPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?xm)
        (?:
            ^\*\*\s\(\w                        # Elixir:  ** (RuntimeError)
          | ^Traceback\s\(most\srecent\scall\slast\):
          | ^panic:\s                          # Go
          | ^fatal\serror:\s                   # Go runtime
          | ^thread\s'.*'\spanicked\sat        # Rust
          | \b[A-Z]\w*(?:Error|Exception)\b    # FooError / BarException anywhere
          | \bunhandled\s(?:exception|rejection|error)\b
          | \bsegmentation\sfault\b
          | \bstack\soverflow\b
          | \bcore\sdumped\b
          | ^\s*Caused\sby:
          | \[error\]                          # explicit, cheap, common
          | \bERR!                             # npm
          | \bCompilationError\b
          | \bcommand\snot\sfound\b
          | \bAddress\salready\sin\suse\b
          | \bECONN(?:REFUSED|RESET)\b
          | \bEADDRINUSE\b
          | \bMODULE_NOT_FOUND\b
        )",
    )
    .expect("EXCEPTION regex")
});

static WARNING_SHAPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?xm)(?:^\s*warning:|\bdeprecated\b|\bDeprecationWarning\b|\bWARN!\b)")
        .expect("WARNING_SHAPE regex")
});

/// What the classifier concluded, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub level: Level,
    pub is_error: bool,
    /// Comma-separated signal names. Empty when nothing fired.
    pub reasons: Vec<&'static str>,
}

impl Classification {
    pub fn reason_string(&self) -> Option<String> {
        if self.reasons.is_empty() {
            None
        } else {
            Some(self.reasons.join(","))
        }
    }
}

/// Classify one grouped entry from its rendered text.
pub fn classify(text: &str) -> Classification {
    let mut reasons = Vec::new();

    let tagged = tagged_level(text);
    let mut level = tagged.unwrap_or(Level::Unknown);

    if tagged == Some(Level::Error) {
        reasons.push("level_tag");
    }
    if EXCEPTION.is_match(text) {
        reasons.push("exception_marker");
    }
    if stack_frames(text) >= 2 {
        reasons.push("stack_frames");
    }

    // JSON logs carry their own level field and deserve to be believed.
    if tagged.is_none() {
        if let Some(l) = json_level(text) {
            level = l;
            if l == Level::Error {
                reasons.push("level_tag");
            }
        }
    }

    let is_error = !reasons.is_empty();
    if is_error {
        level = Level::Error;
    } else if level == Level::Unknown && WARNING_SHAPE.is_match(text) {
        level = Level::Warn;
    }

    Classification {
        level,
        is_error,
        reasons,
    }
}

/// Signals that belong to the run rather than to any one entry. Folded in by the daemon
/// when the process ends, so a silent crash still registers as a failure.
pub fn run_failed(exit_code: Option<i32>, signal: Option<&str>) -> Option<&'static str> {
    // Ctrl-C is how you stop a dev server, and it exits 130. Checking the signal first is
    // what stops every deliberate shutdown from being reported as a crash.
    if matches!(signal, Some("SIGINT") | Some("SIGTERM") | Some("SIGHUP")) {
        return None;
    }
    match (exit_code, signal) {
        (_, Some(_)) => Some("killed_by_signal"),
        (Some(c), _) if c != 0 => Some("nonzero_exit"),
        (None, None) => Some("unexpected_death"),
        _ => None,
    }
}

fn tagged_level(text: &str) -> Option<Level> {
    let first = text.lines().next().unwrap_or("");
    let caps = LEVEL_TAG.captures(first)?;
    for name in ["a", "b", "c", "e"] {
        if let Some(m) = caps.name(name) {
            return Some(Level::parse(m.as_str()));
        }
    }
    // Ruby's single-letter severity.
    caps.name("d").map(|m| match m.as_str() {
        "D" => Level::Debug,
        "I" => Level::Info,
        "W" => Level::Warn,
        "E" | "F" => Level::Error,
        _ => Level::Unknown,
    })
}

fn json_level(text: &str) -> Option<Level> {
    let first = text.lines().next()?.trim();
    if !first.starts_with('{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(first).ok()?;
    for key in ["level", "lvl", "severity", "levelname"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            return Some(Level::parse(s));
        }
        // Bunyan and pino use numeric levels.
        if let Some(n) = v.get(key).and_then(|x| x.as_i64()) {
            return Some(match n {
                0..=19 => Level::Trace,
                20..=29 => Level::Debug,
                30..=39 => Level::Info,
                40..=49 => Level::Warn,
                _ => Level::Error,
            });
        }
    }
    None
}

fn stack_frames(text: &str) -> usize {
    text.lines()
        .filter(|l| super::group::is_continuation(l))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lvl(s: &str) -> Level {
        classify(s).level
    }

    fn is_err(s: &str) -> bool {
        classify(s).is_error
    }

    #[test]
    fn explicit_level_tags_are_read() {
        assert_eq!(lvl("12:04:11.882 [info] GET /users"), Level::Info);
        assert_eq!(lvl("12:04:11.882 [debug] QUERY OK source=\"users\""), Level::Debug);
        assert_eq!(lvl("INFO:root:GET /users"), Level::Info);
        assert_eq!(lvl("level=warn msg=\"slow query\""), Level::Warn);
        assert_eq!(lvl("I, [2026-07-26T12:04:11 #1]  INFO -- : GET /users"), Level::Info);
    }

    #[test]
    fn every_ecosystems_exception_shape_is_caught() {
        for (label, text) in [
            ("elixir", "** (Ecto.NoResultsError) expected at least one result\n    (my_app) lib/users.ex:42: Users.get!/1"),
            ("python", "Traceback (most recent call last):\n  File \"/app/m.py\", line 1, in <module>\nValueError: boom"),
            ("node", "Error: boom\n    at Object.<anonymous> (/app/s.js:1:1)\n    at Module._compile (node:internal:1:1)"),
            ("go", "panic: runtime error: index out of range [3]\n\tmain.main()"),
            ("rust", "thread 'main' panicked at src/main.rs:4:5:\nboom"),
            ("ruby", "boom one\n-e:5:in '<main>'\n        from -e:2:in 'block'"),
            ("npm", "npm ERR! code ELIFECYCLE"),
            ("port", "Address already in use - bind(2) for 127.0.0.1:4000"),
        ] {
            assert!(is_err(text), "missed the {label} error:\n{text}");
            assert_eq!(classify(text).level, Level::Error, "{label}");
        }
    }

    #[test]
    fn a_detected_error_always_says_why() {
        let c = classify("** (RuntimeError) boom\n    (my_app) lib/a.ex:1: A.b/0");
        assert!(c.is_error);
        assert!(c.reason_string().is_some());
        assert!(c.reasons.contains(&"exception_marker"), "{:?}", c.reasons);
    }

    #[test]
    fn ordinary_lines_are_not_errors() {
        for text in [
            "12:04:11.882 [info] GET /users",
            "compiled 41 files in 2.3s",
            "Server listening on http://localhost:4000",
            "QUERY OK source=\"users\" db=1.2ms",
            "  ✓ renders the page (12ms)",
        ] {
            assert!(!is_err(text), "false positive: {text}");
        }
    }

    #[test]
    fn json_logs_are_believed() {
        assert_eq!(lvl(r#"{"level":"warn","msg":"slow"}"#), Level::Warn);
        assert!(is_err(r#"{"level":"error","msg":"boom"}"#));
        // pino numeric levels
        assert_eq!(lvl(r#"{"level":30,"msg":"hi"}"#), Level::Info);
        assert!(is_err(r#"{"level":50,"msg":"bad"}"#));
    }

    #[test]
    fn warnings_are_warnings_not_errors() {
        let c = classify("warning: unused variable `x`");
        assert_eq!(c.level, Level::Warn);
        assert!(!c.is_error);
    }

    #[test]
    fn run_level_signals_are_independent_of_output() {
        assert_eq!(run_failed(Some(1), None), Some("nonzero_exit"));
        assert_eq!(run_failed(Some(0), None), None);
        assert_eq!(run_failed(None, Some("SIGSEGV")), Some("killed_by_signal"));
        // Ctrl-C is how you stop a dev server. It is not a failure.
        assert_eq!(run_failed(Some(130), Some("SIGINT")), None);
        assert_eq!(run_failed(None, None), Some("unexpected_death"));
    }
}

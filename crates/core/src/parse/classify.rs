//! Level assignment and error detection.
//!
//! Severity itself is decided by the single ladder in [`super::severity`]; this module
//! only decides what a severity means:
//!
//! * an error is a declared error/fatal, or a structural exception banner;
//! * an `unknown` severity is never an error, however alarming the text reads;
//! * run-level failure (non-zero exit, fatal signal) is folded in separately by the
//!   daemon, since it is a fact about the process rather than about any line.
//!
//! Text-shape heuristics are not used to raise severity — on real server output they
//! produced overwhelmingly false errors. Such lines are still stored and searchable, and
//! are surfaced as novelty instead; see `store::write`.

use crate::model::Level;
use crate::parse::severity::{self, Evidence};

/// What the classifier concluded, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub level: Level,
    pub evidence: Evidence,
    pub is_error: bool,
    /// Which rule answered. `None` only when nothing declared anything.
    pub signal: Option<&'static str>,
}

impl Classification {
    /// Stored on the entry as `rung:rule`, e.g. `declared:level_token`.
    pub fn reason_string(&self) -> Option<String> {
        self.signal
            .map(|s| format!("{}:{}", self.evidence.as_str(), s))
    }
}

/// Classify one grouped entry from its rendered text.
pub fn classify(text: &str) -> Classification {
    let s = severity::detect(text);
    let is_error = match s.evidence {
        // A structural banner is the runtime reporting a throw.
        Evidence::Structural => true,
        Evidence::Declared => s.level == Level::Error,
        Evidence::Unknown => false,
    };
    Classification {
        level: s.level,
        evidence: s.evidence,
        is_error,
        signal: s.signal,
    }
}

/// Whether the run ended because someone stopped it, rather than by failing.
///
/// Ctrl-C and a stop from outside are how a dev server normally ends. Some processes die of
/// the signal, and others catch it and exit the way a shell would report it.
///
/// [`crate::store::schema::STOPPED_BY_USER_SQL`] is the same rule for queries.
pub fn stopped_by_user(exit_code: Option<i32>, signal: Option<&str>) -> bool {
    matches!(signal, Some("SIGINT" | "SIGTERM" | "SIGHUP")) || matches!(exit_code, Some(129 | 130 | 143))
}

/// Reports run-level failure from the process exit status, so a silent crash registers
/// even when the process printed nothing.
///
/// Called by the daemon when the process ends.
pub fn run_failed(exit_code: Option<i32>, signal: Option<&str>) -> Option<&'static str> {
    // Checked first: Ctrl-C exits 130, which would otherwise look like a crash.
    if stopped_by_user(exit_code, signal) {
        return None;
    }
    match (exit_code, signal) {
        (_, Some(_)) => Some("killed_by_signal"),
        (Some(c), _) if c != 0 => Some("nonzero_exit"),
        (None, None) => Some("unexpected_death"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopping_a_server_is_not_a_failure_however_it_reports_it() {
        for (code, signal) in [
            (Some(1), Some("SIGINT")),
            (Some(1), Some("SIGTERM")),
            (Some(1), Some("SIGHUP")),
            (Some(130), None),
            (Some(143), None),
            (Some(129), None),
        ] {
            assert!(stopped_by_user(code, signal), "{code:?} {signal:?}");
            assert_eq!(run_failed(code, signal), None, "{code:?} {signal:?}");
        }
    }

    #[test]
    fn a_crash_is_still_a_failure() {
        for (code, signal) in [
            (Some(1), None),
            (Some(137), None),
            (Some(1), Some("SIGSEGV")),
            (Some(1), Some("SIGKILL")),
            (None, None),
        ] {
            assert!(!stopped_by_user(code, signal), "{code:?} {signal:?}");
            assert!(run_failed(code, signal).is_some(), "{code:?} {signal:?}");
        }
    }

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
            ("npm", "npm ERR! code ELIFECYCLE"),
            ("declared", "12:04:11 [error] Address already in use - bind(2) for 127.0.0.1:4000"),
        ] {
            assert!(is_err(text), "missed the {label} error:\n{text}");
            assert_eq!(classify(text).level, Level::Error, "{label}");
        }
    }

    #[test]
    fn a_detected_error_always_says_which_rung_answered() {
        let c = classify("** (RuntimeError) boom\n    (my_app) lib/a.ex:1: A.b/0");
        assert!(c.is_error);
        assert_eq!(c.evidence, Evidence::Structural);
        assert_eq!(c.reason_string().as_deref(), Some("structural:elixir_exception"));
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
    fn a_phoenix_request_log_is_not_an_error() {
        let c = classify("[debug] Processing with EnlightenWeb.Page.HomeLive.__live__/2\n  Parameters: %{}\n  Pipelines: [:browser]");
        assert!(!c.is_error);
        assert_eq!(c.level, Level::Debug);
        assert_eq!(c.evidence, Evidence::Declared);
    }

    #[test]
    fn unknown_severity_is_never_an_error() {
        for text in [
            "%{user_id: 42}",
            "HERE",
            "some output with no level at all",
            "  indented\n  more indented\n  even more",
        ] {
            let c = classify(text);
            assert!(!c.is_error, "{text}");
            assert_eq!(c.level, Level::Unknown, "{text}");
            assert_eq!(c.evidence, Evidence::Unknown, "{text}");
        }
    }

    #[test]
    fn json_logs_are_believed() {
        assert_eq!(lvl(r#"{"level":"warn","msg":"slow"}"#), Level::Warn);
        assert!(is_err(r#"{"level":"error","msg":"boom"}"#));
        // pino's numeric scale, on which 30 is info
        assert_eq!(lvl(r#"{"level":30,"msg":"hi"}"#), Level::Info);
        assert!(is_err(r#"{"level":50,"msg":"bad"}"#));
    }

    #[test]
    fn warnings_are_warnings_not_errors() {
        let c = classify("[warning] unused variable `x`");
        assert_eq!(c.level, Level::Warn);
        assert!(!c.is_error);
    }

    #[test]
    fn run_level_signals_are_independent_of_output() {
        assert_eq!(run_failed(Some(1), None), Some("nonzero_exit"));
        assert_eq!(run_failed(Some(0), None), None);
        assert_eq!(run_failed(None, Some("SIGSEGV")), Some("killed_by_signal"));
        assert_eq!(run_failed(Some(130), Some("SIGINT")), None);
        assert_eq!(run_failed(None, None), Some("unexpected_death"));
    }
}

//! Secret redaction, applied **on the way in**.
//!
//! Capturing everything a developer runs means capturing their secrets. The only version
//! of this that actually holds is redaction before the write: data that never lands cannot
//! leak through an export, a sync, a bug in a query, or an agent reading a log.
//!
//! Redaction is length-preserving where it can be. Rewriting a token to a shorter marker
//! would shift every byte after it, and `chunks` is supposed to replay byte-exact into a
//! terminal. So a matched secret becomes the same number of `•` characters, and the stream
//! stays aligned.

use regex::bytes::{Regex, RegexSet};
use std::sync::OnceLock;

/// Patterns matched against raw bytes. Ordered roughly by confidence: shapes that cannot
/// plausibly be anything else come first.
const PATTERNS: &[&str] = &[
    // AWS access key id / secret pairs
    r"\b(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b",
    r"(?i)\baws_secret_access_key\s*[=:]\s*\S+",
    // GitHub, GitLab, Slack, Stripe, OpenAI, Anthropic token shapes
    r"\bgh[pousr]_[A-Za-z0-9]{16,}\b",
    r"\bglpat-[A-Za-z0-9_\-]{16,}\b",
    r"\bxox[abposr]-[A-Za-z0-9\-]{10,}\b",
    r"\b[rs]k_(?:live|test)_[A-Za-z0-9]{16,}\b",
    r"\bsk-(?:proj-|ant-)?[A-Za-z0-9_\-]{20,}\b",
    // JWTs
    r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\b",
    // Authorization headers
    r"(?i)\b(?:authorization|proxy-authorization)\s*[=:]\s*(?:bearer|basic|token)\s+\S+",
    r"(?i)\bbearer\s+[A-Za-z0-9._\-]{20,}",
    // Connection strings carrying credentials
    r"(?i)\b[a-z][a-z0-9+.\-]*://[^\s:@/]+:[^\s@/]+@",
    // PEM private key blocks
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----(?s:.*?)-----END [A-Z ]*PRIVATE KEY-----",
    // KEY=value where the key name announces itself
    r"(?i)\b[A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|APIKEY|API_KEY|PRIVATE_KEY|CREDENTIAL|ACCESS_KEY|AUTH)[A-Z0-9_]*\s*[=:]\s*(?:\x22[^\x22]*\x22|'[^']*'|[^\s;,]+)",
];

/// The replacement glyph. U+2022 is three bytes in UTF-8, so length preservation is done
/// on a byte budget rather than a character count.
const DOT: &[u8] = "•".as_bytes();

pub struct Redactor {
    set: RegexSet,
    regexes: Vec<Regex>,
    enabled: bool,
}

impl Redactor {
    pub fn new(enabled: bool, extra: &[String]) -> Redactor {
        let mut sources: Vec<String> = PATTERNS.iter().map(|s| s.to_string()).collect();
        for p in extra {
            // A bad user pattern must not take down capture. Drop it and carry on.
            if Regex::new(p).is_ok() {
                sources.push(p.clone());
            }
        }
        let regexes: Vec<Regex> = sources.iter().filter_map(|s| Regex::new(s).ok()).collect();
        let set = RegexSet::new(sources.iter().filter(|s| Regex::new(s).is_ok()))
            .unwrap_or_else(|_| RegexSet::empty());
        Redactor {
            set,
            regexes,
            enabled,
        }
    }

    pub fn disabled() -> Redactor {
        Redactor {
            set: RegexSet::empty(),
            regexes: Vec::new(),
            enabled: false,
        }
    }

    /// Redact in place. Returns the number of secrets masked.
    ///
    /// The `RegexSet` prefilter is the reason this is affordable on the ingest hot path:
    /// the overwhelmingly common case is a line containing no secret at all, and that case
    /// costs one pass with no capture-group bookkeeping.
    pub fn redact(&self, buf: &mut Vec<u8>) -> usize {
        if !self.enabled || buf.is_empty() {
            return 0;
        }
        let hits: Vec<usize> = self.set.matches(buf).into_iter().collect();
        if hits.is_empty() {
            return 0;
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for i in hits {
            for m in self.regexes[i].find_iter(buf) {
                spans.push((m.start(), m.end()));
            }
        }
        if spans.is_empty() {
            return 0;
        }
        spans.sort_unstable();
        // Merge overlaps so nested matches are masked once.
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (s, e) in spans {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        for &(s, e) in merged.iter().rev() {
            let mask = mask_of(e - s);
            buf.splice(s..e, mask);
        }
        merged.len()
    }

    /// Convenience for text paths (settings echoes, notification bodies).
    pub fn redact_str(&self, s: &str) -> String {
        let mut buf = s.as_bytes().to_vec();
        self.redact(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }
}

fn mask_of(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() + DOT.len() <= len {
        out.extend_from_slice(DOT);
    }
    // Pad the remainder so the byte length is preserved exactly.
    while out.len() < len {
        out.push(b'.');
    }
    out
}

/// A process-wide default for callers that have no settings handy.
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
}

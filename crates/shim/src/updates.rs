//! `logbear updates` — the one network request logbear can make, and it is off by default.
//!
//! When turned on, at most once a day, `logbear doctor` asks GitHub for the latest release:
//! one HTTPS GET to the releases API, carrying nothing but a `User-Agent` with the installed
//! version. Nothing about projects, runs or output is sent, and nothing else in logbear
//! touches the network. It goes through the system `curl` rather than an HTTP client
//! compiled into every binary, so the whole of logbear's network code is this file.

use crate::ui;
use logbear_core::paths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const RELEASES: &str = "https://api.github.com/repos/teralabx/logbear/releases/latest";
const DAY_MICROS: i64 = 24 * 60 * 60 * 1_000_000;
const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub enabled: bool,
    /// When GitHub last answered, in micros. 0 if it never has.
    #[serde(default)]
    pub checked_at: i64,
    /// The newest release GitHub reported, without a leading `v`.
    #[serde(default)]
    pub latest: Option<String>,
}

fn file() -> PathBuf {
    paths::home().join("updates.json")
}

pub fn load() -> State {
    std::fs::read_to_string(file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(state: &State) -> Result<(), String> {
    let _ = paths::ensure_home();
    let body = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(file(), body + "\n").map_err(|e| format!("{}: {e}", file().display()))
}

/// `logbear updates [on|off|check]`.
pub fn run(action: Option<String>) -> Result<i32, String> {
    let mut state = load();
    match action.as_deref() {
        None => {}
        Some("on") => {
            state.enabled = true;
            save(&state)?;
            println!(
                "Update checks are on. Once a day at most, `logbear doctor` sends one request to\n  \
                 {RELEASES}\nwith the header `User-Agent: logbear/{CURRENT}`, and nothing else."
            );
        }
        Some("off") => {
            state.enabled = false;
            save(&state)?;
            println!("Update checks are off. logbear makes no network requests.");
            return Ok(0);
        }
        Some("check") => {
            // Asked for by name, so it runs whether or not daily checks are on.
            check_now(&mut state)?;
            save(&state)?;
        }
        Some(other) => {
            return Err(format!(
                "logbear updates: expected on, off or check, not {other:?}"
            ))
        }
    }
    println!("{}", describe(&state, logbear_core::now_micros()));
    Ok(0)
}

/// The `doctor` line, checking first when checks are on and the last answer is a day old.
/// A failed check is reported, never fatal: being offline is not a broken install.
pub fn for_doctor() -> String {
    let mut state = load();
    let now = logbear_core::now_micros();
    if state.enabled && is_stale(state.checked_at, now) {
        match check_now(&mut state) {
            Ok(()) => {
                let _ = save(&state);
            }
            Err(e) => return format!("could not reach GitHub ({e})"),
        }
    }
    describe(&state, now)
}

fn check_now(state: &mut State) -> Result<(), String> {
    let url = std::env::var("LOGBEAR_UPDATE_URL").unwrap_or_else(|_| RELEASES.to_string());
    let out = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "5",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
        ])
        .arg(format!("User-Agent: logbear/{CURRENT}"))
        .arg(&url)
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let latest = parse_latest(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| "no tag_name in GitHub's reply".to_string())?;
    state.latest = Some(latest);
    state.checked_at = logbear_core::now_micros();
    Ok(())
}

fn describe(state: &State, now: i64) -> String {
    if !state.enabled && state.latest.is_none() {
        return format!(
            "v{CURRENT} · update checks off (`logbear updates on` to check GitHub daily)"
        );
    }
    let when = if state.checked_at > 0 {
        format!("checked {}", ui::rel_time_from(now, state.checked_at))
    } else {
        "not checked yet".to_string()
    };
    match &state.latest {
        Some(latest) if newer(latest, CURRENT) => {
            format!(
                "v{latest} is available (you have v{CURRENT}) · {}",
                upgrade_hint()
            )
        }
        Some(_) => format!("v{CURRENT} is the latest · {when}"),
        None => format!("v{CURRENT} · {when}"),
    }
}

/// How to upgrade depends on how this copy got here.
fn upgrade_hint() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    let exe = exe.to_string_lossy();
    if logbear_setup::homebrew_managed() || exe.contains("/Cellar/") {
        "brew upgrade logbear".into()
    } else if exe.contains(".app/Contents/MacOS") {
        "download the new app from https://github.com/teralabx/logbear/releases".into()
    } else {
        "curl -fsSL https://raw.githubusercontent.com/teralabx/logbear/master/install.sh | sh"
            .into()
    }
}

fn is_stale(checked_at: i64, now: i64) -> bool {
    now - checked_at >= DAY_MICROS
}

/// The release's version, without its leading `v`.
fn parse_latest(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = v.get("tag_name")?.as_str()?;
    Some(tag.trim_start_matches('v').to_string())
}

/// Whether `a` is a later release than `b`. Pre-release suffixes are ignored: a stable
/// install is never told to move to a release candidate of the version it already has.
fn newer(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    }
    parts(a) > parts(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_not_as_text() {
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("1.0.0", "0.99.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0-rc.1", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn the_tag_is_read_from_githubs_reply() {
        let body = r#"{"tag_name":"v0.2.0","name":"logbear 0.2.0","draft":false}"#;
        assert_eq!(parse_latest(body).as_deref(), Some("0.2.0"));
        assert_eq!(parse_latest(r#"{"message":"Not Found"}"#), None);
        assert_eq!(parse_latest("<html>"), None);
    }

    #[test]
    fn a_day_old_answer_is_stale_and_a_fresh_one_is_not() {
        let now = 10 * DAY_MICROS;
        assert!(is_stale(0, now), "never checked");
        assert!(is_stale(now - DAY_MICROS, now));
        assert!(!is_stale(now - DAY_MICROS / 2, now));
    }

    #[test]
    fn off_says_so_and_how_to_turn_it_on() {
        let text = describe(&State::default(), 0);
        assert!(
            text.contains("off") && text.contains("logbear updates on"),
            "{text}"
        );
    }

    #[test]
    fn a_newer_release_is_announced_with_how_to_get_it() {
        let state = State {
            enabled: true,
            checked_at: 1,
            latest: Some("99.0.0".into()),
        };
        let text = describe(&state, 2);
        assert!(text.contains("v99.0.0 is available"), "{text}");
    }
}

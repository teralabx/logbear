//! `runlog doctor` — why isn't it working?
//!
//! The install has four moving parts that live outside runlog's own files: a binary on
//! `PATH`, a daemon that has to be running, an MCP entry in each harness's config, and a
//! hook block in each harness's config. All four are edited by the installer, all four can
//! be undone by a shell profile change or a harness upgrade, and none of them announce
//! themselves when they break — the failure mode is "the agent stopped noticing my errors"
//! with no error anywhere.
//!
//! So every check reports one of ok / missing / broken plus a single line saying what to do
//! about it, and the process exits non-zero if anything is not ok. That exit code is what
//! makes this usable from a script and from the app's own diagnostics panel.

use crate::ui::{self, Cell};
use runlog_core::client::Client;
use runlog_core::paths;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Ok,
    /// Not set up. Actionable, and the usual state on a fresh machine.
    Missing,
    /// Set up but wrong — a config that will not parse, a daemon that will not answer.
    /// Worse than missing, because the user believes it is working.
    Broken,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Missing => "missing",
            State::Broken => "broken",
        }
    }

    fn colour(self) -> &'static str {
        match self {
            State::Ok => ui::CLEAR,
            State::Missing => ui::EMBER,
            State::Broken => ui::FLARE,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub state: State,
    pub detail: String,
    /// Empty when there is nothing to fix.
    pub fix: String,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Check {
        Check {
            name,
            state: State::Ok,
            detail: detail.into(),
            fix: String::new(),
        }
    }

    fn missing(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
        Check {
            name,
            state: State::Missing,
            detail: detail.into(),
            fix: fix.into(),
        }
    }

    fn broken(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
        Check {
            name,
            state: State::Broken,
            detail: detail.into(),
            fix: fix.into(),
        }
    }
}

pub fn run(json: bool) -> Result<i32, String> {
    let checks = collect();
    let failures = checks.iter().filter(|c| c.state != State::Ok).count();

    if json {
        let text = serde_json::to_string_pretty(&serde_json::json!({
            "checks": checks,
            "failures": failures,
        }))
        .map_err(|e| format!("runlog: {e}"))?;
        println!("{text}");
        return Ok(if failures == 0 { 0 } else { 1 });
    }

    let rows: Vec<Vec<Cell>> = checks
        .iter()
        .map(|c| {
            vec![
                Cell::coloured(c.state.label(), c.state.colour()),
                Cell::plain(c.name),
                Cell::coloured(c.detail.clone(), ui::ASH),
            ]
        })
        .collect();
    print!("{}", ui::table(&["", "CHECK", "DETAIL"], &rows));

    if failures == 0 {
        println!("\n{}", ui::clear("everything checks out"));
        return Ok(0);
    }
    println!();
    for check in checks.iter().filter(|c| c.state != State::Ok && !c.fix.is_empty()) {
        println!("{} {}", ui::ash(&format!("{}:", check.name)), check.fix);
    }
    Ok(1)
}

fn collect() -> Vec<Check> {
    vec![
        check_on_path(),
        check_daemon_binary(),
        check_daemon(),
        check_mcp_claude_code(),
        check_hook_claude_code(),
        check_mcp_codex(),
        check_hook_codex(),
    ]
}

// ---------------------------------------------------------------------------
// Binaries and daemon
// ---------------------------------------------------------------------------

fn check_on_path() -> Check {
    let exe = std::env::current_exe().ok();
    match which("runlog") {
        Some(found) => {
            let same = exe
                .as_ref()
                .and_then(|e| e.canonicalize().ok())
                .zip(found.canonicalize().ok())
                .map(|(a, b)| a == b)
                .unwrap_or(true);
            if same {
                Check::ok("runlog on PATH", found.display().to_string())
            } else {
                // Two installs is the quiet way to spend an afternoon debugging a fix that
                // was never running.
                Check::broken(
                    "runlog on PATH",
                    format!(
                        "PATH has {}, you are running {}",
                        found.display(),
                        exe.map(|e| e.display().to_string()).unwrap_or_default()
                    ),
                    "remove the stale copy, or re-link PATH at the one you want",
                )
            }
        }
        None => Check::missing(
            "runlog on PATH",
            "not found on PATH",
            format!(
                "ln -s {} /usr/local/bin/runlog",
                exe.map(|e| e.display().to_string())
                    .unwrap_or_else(|| "<this binary>".into())
            ),
        ),
    }
}

fn check_daemon_binary() -> Check {
    match runlog_core::client::daemon_binary() {
        Some(path) => Check::ok("runlogd binary", path.display().to_string()),
        None => Check::missing(
            "runlogd binary",
            "not next to runlog and not on PATH",
            "reinstall from the runlog app, or set RUNLOG_DAEMON to its path",
        ),
    }
}

fn check_daemon() -> Check {
    let socket = paths::socket();
    match Client::connect() {
        Ok(mut client) => match client.ping() {
            Ok(()) => match client.status() {
                Ok(status) => Check::ok(
                    "daemon",
                    format!("v{} · pid {} · {}", status.version, status.pid, socket.display()),
                ),
                Err(e) => Check::ok("daemon", format!("responding to ping ({e})")),
            },
            // A socket that accepts a connection and then does not answer is the worst
            // case: everything looks installed and nothing works.
            Err(e) => Check::broken(
                "daemon",
                format!("socket accepted the connection but did not answer: {e}"),
                "kill the stale daemon and let the next `runlog` command restart it",
            ),
        },
        Err(_) if socket.exists() => Check::broken(
            "daemon",
            format!("a stale socket is sitting at {}", socket.display()),
            format!("rm {} and run any `runlog` command", socket.display()),
        ),
        Err(_) => Check::missing(
            "daemon",
            format!("not running (no socket at {})", socket.display()),
            "start it with `runlogd serve`, or open the runlog app",
        ),
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// Harness configs
// ---------------------------------------------------------------------------

/// Claude Code keeps user-level MCP servers in `~/.claude.json`; a project can also
/// declare them in `.mcp.json`. Both are checked, because "it works in this repo" and "it
/// works everywhere" are different answers and the user means one of them.
fn check_mcp_claude_code() -> Check {
    let candidates = [
        home().join(".claude.json"),
        home().join(".claude/settings.json"),
        PathBuf::from(".mcp.json"),
    ];
    scan_json(
        "MCP · Claude Code",
        &candidates,
        |value| {
            value
                .get("mcpServers")
                .and_then(|s| s.get("runlog"))
                .is_some()
        },
        "register it from the runlog app, or add an `mcpServers.runlog` entry to ~/.claude.json",
    )
}

/// Hooks live in `~/.claude/settings.json`, and what matters is that some hook command
/// actually invokes `runlog-hook` — the block's shape is the harness's business and
/// changes between versions, so this looks for the binary name rather than a schema.
fn check_hook_claude_code() -> Check {
    let candidates = [
        home().join(".claude/settings.json"),
        home().join(".claude/settings.local.json"),
    ];
    scan_json(
        "hooks · Claude Code",
        &candidates,
        |value| mentions(value.get("hooks"), "runlog-hook"),
        "install the hook from the runlog app (it merges into ~/.claude/settings.json, never overwrites)",
    )
}

fn check_mcp_codex() -> Check {
    let path = home().join(".codex/config.toml");
    if !path.exists() {
        return Check::missing(
            "MCP · Codex",
            "no ~/.codex/config.toml — Codex does not look installed",
            "install Codex first, then register runlog from the runlog app",
        );
    }
    match std::fs::read_to_string(&path) {
        // Matched textually rather than by parsing TOML: this is a presence check, and
        // adding a TOML parser to the shim to answer it would be a poor trade.
        Ok(text) if text.contains("mcp_servers.runlog") => {
            Check::ok("MCP · Codex", path.display().to_string())
        }
        Ok(_) => Check::missing(
            "MCP · Codex",
            format!("no [mcp_servers.runlog] in {}", path.display()),
            "register it from the runlog app",
        ),
        Err(e) => Check::broken(
            "MCP · Codex",
            format!("cannot read {}: {e}", path.display()),
            "check the file's permissions",
        ),
    }
}

fn check_hook_codex() -> Check {
    let candidates = [
        home().join(".codex/hooks.json"),
        PathBuf::from(".codex/hooks.json"),
    ];
    scan_json(
        "hooks · Codex",
        &candidates,
        |value| mentions(Some(value), "runlog-hook"),
        "install the hook from the runlog app (it merges into ~/.codex/hooks.json)",
    )
}

/// Looks through a list of candidate config files for one that satisfies `found`.
///
/// A file that exists but does not parse is reported as **broken**, not missing: a config
/// with a trailing comma in it silently disables every hook in the file, and telling the
/// user "not installed" when the truth is "your JSON is invalid" sends them to fix the
/// wrong thing.
fn scan_json(
    name: &'static str,
    candidates: &[PathBuf],
    found: impl Fn(&serde_json::Value) -> bool,
    fix: &str,
) -> Check {
    let mut seen_any = false;
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        seen_any = true;
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) if found(&value) => return Check::ok(name, path.display().to_string()),
            Ok(_) => continue,
            Err(e) => {
                return Check::broken(
                    name,
                    format!("{} is not valid JSON: {e}", path.display()),
                    "fix the JSON — while it is invalid the harness ignores the whole file",
                )
            }
        }
    }
    let detail = if seen_any {
        format!("not registered in {}", describe(candidates))
    } else {
        format!("no config found at {}", describe(candidates))
    };
    Check::missing(name, detail, fix)
}

fn describe(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| tilde(p))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn tilde(path: &Path) -> String {
    let text = path.display().to_string();
    let home = home().display().to_string();
    match text.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => text,
    }
}

/// Whether `needle` appears anywhere in the subtree, at any depth. Hook config is nested
/// differently in every harness version; what stays constant is that our binary's name has
/// to appear somewhere in it.
fn mentions(value: Option<&serde_json::Value>, needle: &str) -> bool {
    match value {
        None => false,
        Some(serde_json::Value::String(s)) => s.contains(needle),
        Some(serde_json::Value::Array(items)) => items.iter().any(|v| mentions(Some(v), needle)),
        Some(serde_json::Value::Object(map)) => map.values().any(|v| mentions(Some(v), needle)),
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hook_command_is_found_however_it_is_nested() {
        let config = serde_json::json!({
            "hooks": {
                "Stop": [{ "hooks": [{ "type": "command", "command": "runlog-hook stop" }] }]
            }
        });
        assert!(mentions(config.get("hooks"), "runlog-hook"));
        assert!(!mentions(config.get("hooks"), "some-other-hook"));
        assert!(!mentions(None, "runlog-hook"));
    }

    #[test]
    fn an_unrelated_config_is_not_a_false_positive() {
        let config = serde_json::json!({ "hooks": { "Stop": [] }, "model": "opus" });
        assert!(!mentions(config.get("hooks"), "runlog-hook"));
    }

    #[test]
    fn invalid_json_is_broken_rather_than_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ \"hooks\": , }").unwrap();
        let check = scan_json("t", &[path], |_| true, "fix");
        assert_eq!(check.state, State::Broken);
        assert!(check.detail.contains("not valid JSON"), "{}", check.detail);
    }

    #[test]
    fn a_registered_entry_is_ok_and_an_absent_one_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.json");
        std::fs::write(&good, r#"{"mcpServers":{"runlog":{"command":"runlog-mcp"}}}"#).unwrap();
        let bare = dir.path().join("bare.json");
        std::fs::write(&bare, r#"{"mcpServers":{"other":{}}}"#).unwrap();

        let has_runlog =
            |v: &serde_json::Value| v.get("mcpServers").and_then(|s| s.get("runlog")).is_some();
        assert_eq!(scan_json("t", &[good], has_runlog, "fix").state, State::Ok);
        assert_eq!(scan_json("t", &[bare], has_runlog, "fix").state, State::Missing);
    }

    #[test]
    fn a_missing_file_is_missing_not_broken() {
        let check = scan_json("t", &[PathBuf::from("/nonexistent/runlog-test.json")], |_| true, "fix");
        assert_eq!(check.state, State::Missing);
        assert!(!check.fix.is_empty(), "a failing check must say what to do");
    }

    #[test]
    fn every_check_that_fails_offers_a_fix() {
        for check in collect() {
            if check.state != State::Ok {
                assert!(
                    !check.fix.is_empty(),
                    "`{}` reported {:?} with no suggested fix",
                    check.name,
                    check.state
                );
            }
        }
    }
}

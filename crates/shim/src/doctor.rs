//! `logbear doctor` — check the parts of the install that live outside logbear's own files.
//!
//! A binary on `PATH`, a running daemon, an MCP entry and a hook block in each harness's
//! config: all four are written by the installer, all four can be undone by a shell profile
//! change or a harness upgrade, and none announce themselves when they break.
//!
//! Each check reports ok / missing / broken plus one line saying what to do about it, and
//! the process exits non-zero if anything is not ok.

use crate::ui::{self, Cell};
use logbear_core::client::Client;
use logbear_core::paths;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Ok,
    /// Not set up. The usual state on a fresh machine.
    Missing,
    /// Set up but wrong: a config that will not parse, a daemon that will not answer.
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
        .map_err(|e| format!("logbear: {e}"))?;
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
    let mut checks = vec![
        check_on_path(),
        check_daemon_binary(),
        check_daemon(),
        check_mcp_claude_code(),
        check_hook_claude_code(),
        check_mcp_codex(),
        check_hook_codex(),
    ];
    checks.extend(check_pi());
    checks.extend(check_hermes());
    // Informational: an available update, or being offline, is not a broken install.
    checks.push(Check::ok("updates", crate::updates::for_doctor()));
    checks
}

// ---------------------------------------------------------------------------
// Binaries and daemon
// ---------------------------------------------------------------------------

fn check_on_path() -> Check {
    let exe = std::env::current_exe().ok();
    match which("logbear") {
        Some(found) => {
            let same = exe
                .as_ref()
                .and_then(|e| e.canonicalize().ok())
                .zip(found.canonicalize().ok())
                .map(|(a, b)| a == b)
                .unwrap_or(true);
            if same {
                Check::ok("logbear on PATH", found.display().to_string())
            } else {
                // Two installs means debugging a fix that was never running.
                Check::broken(
                    "logbear on PATH",
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
            "logbear on PATH",
            "not found on PATH",
            format!(
                "ln -s {} /usr/local/bin/logbear",
                exe.map(|e| e.display().to_string())
                    .unwrap_or_else(|| "<this binary>".into())
            ),
        ),
    }
}

fn check_daemon_binary() -> Check {
    match logbear_core::client::daemon_binary() {
        Some(path) => Check::ok("logbeard binary", path.display().to_string()),
        None => Check::missing(
            "logbeard binary",
            "not next to logbear and not on PATH",
            "reinstall logbear (brew reinstall logbear, or the install script), or set LOGBEAR_DAEMON to its path",
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
            // A socket that accepts a connection and then does not answer looks installed
            // while nothing works.
            Err(e) => Check::broken(
                "daemon",
                format!("socket accepted the connection but did not answer: {e}"),
                "kill the stale daemon and let the next `logbear` command restart it",
            ),
        },
        Err(_) if socket.exists() => Check::broken(
            "daemon",
            format!("a stale socket is sitting at {}", socket.display()),
            format!("rm {} and run any `logbear` command", socket.display()),
        ),
        Err(_) => Check::missing(
            "daemon",
            format!("not running (no socket at {})", socket.display()),
            "brew services start logbear, or `logbear setup` to register it as a login service",
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

/// Claude Code keeps user-level MCP servers in `~/.claude.json`; a project can also declare
/// them in `.mcp.json`. Both are checked.
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
                .and_then(|s| s.get("logbear"))
                .is_some()
        },
        "logbear setup --agent claude",
    )
}

/// Hooks live in `~/.claude/settings.json`. Matched on the `logbear-hook` binary name rather
/// than on a schema, since the block's shape changes between harness versions.
fn check_hook_claude_code() -> Check {
    let candidates = [
        home().join(".claude/settings.json"),
        home().join(".claude/settings.local.json"),
    ];
    check_hook(
        "hooks · Claude Code",
        &candidates,
        "claude",
        "logbear setup --agent claude",
    )
}

fn check_mcp_codex() -> Check {
    let path = home().join(".codex/config.toml");
    if !path.exists() {
        return Check::missing(
            "MCP · Codex",
            "no ~/.codex/config.toml — Codex does not look installed",
            "install Codex first, then run `logbear setup --agent codex`",
        );
    }
    match std::fs::read_to_string(&path) {
        // A presence check, matched textually rather than by adding a TOML parser to the
        // shim.
        Ok(text) if text.contains("mcp_servers.logbear") => {
            Check::ok("MCP · Codex", tilde(&path))
        }
        Ok(_) => Check::missing(
            "MCP · Codex",
            format!("no [mcp_servers.logbear] in {}", tilde(&path)),
            "logbear setup --agent codex",
        ),
        Err(e) => Check::broken(
            "MCP · Codex",
            format!("cannot read {}: {e}", tilde(&path)),
            "check the file's permissions",
        ),
    }
}

fn check_hook_codex() -> Check {
    let candidates = [
        home().join(".codex/hooks.json"),
        PathBuf::from(".codex/hooks.json"),
    ];
    check_hook(
        "hooks · Codex",
        &candidates,
        "codex",
        "logbear setup --agent codex",
    )
}

// pi and Hermes are checked only where they are installed: a missing harness someone never
// used is not something for `doctor` to fail on.

fn pi_dir() -> PathBuf {
    home().join(".pi/agent")
}

fn check_pi() -> Vec<Check> {
    let dir = pi_dir();
    if !dir.exists() {
        return Vec::new();
    }
    let mcp = scan_json(
        "MCP · pi",
        &[dir.join("mcp.json")],
        |value| {
            value
                .get("mcpServers")
                .and_then(|s| s.get("logbear"))
                .is_some()
        },
        "logbear setup --agent pi (needs pi 1.0 or later, which reads ~/.pi/agent/mcp.json)",
    );
    vec![mcp, check_pi_extension(&dir.join("extensions/logbear.ts"))]
}

/// pi has no hook config; the extension is the registration, and the hook path baked into it
/// is the part that rots when the app moves.
fn check_pi_extension(path: &Path) -> Check {
    const NAME: &str = "hooks · pi";
    const FIX: &str = "logbear setup --agent pi";
    let Ok(source) = std::fs::read_to_string(path) else {
        return Check::missing(NAME, format!("no extension at {}", tilde(path)), FIX);
    };
    let hook = source
        .lines()
        .find_map(|line| line.trim().strip_prefix("const HOOK: string = "))
        .and_then(|rest| serde_json::from_str::<String>(rest.trim_end_matches(';')).ok());
    match hook {
        Some(hook) if Path::new(&hook).exists() => Check::ok(NAME, tilde(path)),
        Some(hook) => Check::broken(
            NAME,
            format!("{} runs {hook}, which no longer exists", tilde(path)),
            "logbear setup --agent pi --remove, then logbear setup --agent pi, so it points at the current binary",
        ),
        None => Check::broken(
            NAME,
            format!("{} does not name a logbear-hook binary", tilde(path)),
            FIX,
        ),
    }
}

/// `HERMES_HOME` selects a profile's directory, as it does for Hermes itself.
fn hermes_home() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".hermes"))
}

fn check_hermes() -> Vec<Check> {
    let home = hermes_home();
    if !home.exists() {
        return Vec::new();
    }
    let text = std::fs::read_to_string(home.join("config.yaml")).ok();
    check_hermes_config(text.as_deref()).into()
}

/// Matched textually, as Codex's TOML is, rather than adding a YAML parser to the shim.
fn check_hermes_config(text: Option<&str>) -> [Check; 2] {
    const FIX_MCP: &str = "logbear setup --agent hermes";
    const FIX_HOOK: &str = "logbear setup --agent hermes, or paste `logbear-hook --emit-config hermes` into config.yaml";
    let Some(text) = text else {
        return [
            Check::missing("MCP · Hermes", "no config.yaml", FIX_MCP),
            Check::missing("hooks · Hermes", "no config.yaml", FIX_HOOK),
        ];
    };

    let mcp = if text.lines().any(|l| l.starts_with("mcp_servers:")) && text.contains("logbear-mcp")
    {
        Check::ok("MCP · Hermes", "config.yaml")
    } else {
        Check::missing(
            "MCP · Hermes",
            "no logbear entry under mcp_servers:",
            FIX_MCP,
        )
    };

    let commands: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("logbear-hook"))
        .collect();
    let foreign = commands
        .iter()
        .find_map(|l| foreign_harness(&serde_json::Value::String((*l).into()), "hermes"));
    let absent: Vec<&str> = ["pre_llm_call", "pre_verify"]
        .into_iter()
        .filter(|event| !text.contains(&format!("{event}:")))
        .collect();
    let hooks = if commands.is_empty() {
        Check::missing("hooks · Hermes", "no logbear-hook in config.yaml", FIX_HOOK)
    } else if let Some(flag) = foreign {
        Check::broken(
            "hooks · Hermes",
            format!("config.yaml runs logbear-hook --harness {flag}"),
            "change it to --harness hermes in config.yaml",
        )
    } else if !absent.is_empty() {
        // Each event does a different job: without pre_verify nothing blocks, without
        // pre_llm_call errors from turns that edited nothing are never told.
        Check::broken(
            "hooks · Hermes",
            format!("config.yaml registers no {}", absent.join(" or ")),
            FIX_HOOK,
        )
    } else {
        Check::ok("hooks · Hermes", "config.yaml")
    };
    [mcp, hooks]
}

/// A presence check, then a check that every registration names this harness.
///
/// A block copied from the other harness's config passes the presence check while every
/// reply comes back in the wrong shape, and Codex treats an unparseable `Stop` reply as a
/// hard hook failure.
fn check_hook(name: &'static str, candidates: &[PathBuf], expected: &str, fix: &str) -> Check {
    let check = scan_json(
        name,
        candidates,
        |value| mentions(value.get("hooks"), "logbear-hook"),
        fix,
    );
    if check.state != State::Ok {
        return check;
    }
    for path in candidates {
        let Some(value) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        else {
            continue;
        };
        if let Some(flag) = foreign_harness(&value, expected) {
            return Check::broken(
                name,
                format!("{} runs logbear-hook --harness {flag}", tilde(path)),
                format!("change it to --harness {expected} in {}", tilde(path)),
            );
        }
    }
    check
}

/// The first `--harness` value on a `logbear-hook` command that is not `expected`.
fn foreign_harness(value: &serde_json::Value, expected: &str) -> Option<String> {
    match value {
        serde_json::Value::String(s) if s.contains("logbear-hook") => {
            let mut words = s.split_whitespace();
            words.find(|w| *w == "--harness")?;
            // A command quoted in YAML or JSON text ends in its closing quote.
            words
                .next()
                .map(|flag| flag.trim_matches(['"', '\'']))
                .filter(|flag| *flag != expected)
                .map(str::to_string)
        }
        serde_json::Value::Array(items) => items.iter().find_map(|v| foreign_harness(v, expected)),
        serde_json::Value::Object(map) => map.values().find_map(|v| foreign_harness(v, expected)),
        _ => None,
    }
}

/// Looks through candidate config files for one that satisfies `found`.
///
/// A file that exists but does not parse is reported as broken rather than missing: invalid
/// JSON silently disables every hook in the file, and "not installed" would send the user
/// to fix the wrong thing.
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
            Ok(value) if found(&value) => return Check::ok(name, tilde(path)),
            Ok(_) => continue,
            Err(e) => {
                return Check::broken(
                    name,
                    format!("{} is not valid JSON: {e}", tilde(path)),
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
/// differently in every harness version.
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
                "Stop": [{ "hooks": [{ "type": "command", "command": "logbear-hook stop" }] }]
            }
        });
        assert!(mentions(config.get("hooks"), "logbear-hook"));
        assert!(!mentions(config.get("hooks"), "some-other-hook"));
        assert!(!mentions(None, "logbear-hook"));
    }

    #[test]
    fn an_unrelated_config_is_not_a_false_positive() {
        let config = serde_json::json!({ "hooks": { "Stop": [] }, "model": "opus" });
        assert!(!mentions(config.get("hooks"), "logbear-hook"));
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
        std::fs::write(&good, r#"{"mcpServers":{"logbear":{"command":"logbear-mcp"}}}"#).unwrap();
        let bare = dir.path().join("bare.json");
        std::fs::write(&bare, r#"{"mcpServers":{"other":{}}}"#).unwrap();

        let has_logbear =
            |v: &serde_json::Value| v.get("mcpServers").and_then(|s| s.get("logbear")).is_some();
        assert_eq!(scan_json("t", &[good], has_logbear, "fix").state, State::Ok);
        assert_eq!(scan_json("t", &[bare], has_logbear, "fix").state, State::Missing);
    }

    #[test]
    fn a_missing_file_is_missing_not_broken() {
        let check = scan_json("t", &[PathBuf::from("/nonexistent/logbear-test.json")], |_| true, "fix");
        assert_eq!(check.state, State::Missing);
        assert!(!check.fix.is_empty(), "a failing check must say what to do");
    }

    #[test]
    fn a_hook_registered_for_the_other_harness_is_broken() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/x/logbear-hook --harness claude"}]}]}}"#,
        )
        .unwrap();
        let check = check_hook("t", std::slice::from_ref(&path), "codex", "fix");
        assert_eq!(check.state, State::Broken);
        assert!(
            check.detail.contains("--harness claude"),
            "{}",
            check.detail
        );
        assert!(!check.fix.is_empty());

        assert_eq!(check_hook("t", &[path], "claude", "fix").state, State::Ok);
    }

    #[test]
    fn a_hook_without_a_harness_flag_is_ok() {
        // The flag is optional: logbear-hook auto-detects the caller when it is absent.
        let config = serde_json::json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": "logbear-hook" }] }] }
        });
        assert_eq!(foreign_harness(&config, "codex"), None);
    }

    #[test]
    fn a_pi_extension_is_judged_by_the_hook_it_points_at() {
        let dir = tempfile::tempdir().unwrap();
        let ext = dir.path().join("logbear.ts");
        assert_eq!(check_pi_extension(&ext).state, State::Missing);

        let hook = dir.path().join("logbear-hook");
        std::fs::write(&hook, "").unwrap();
        let source = |p: &Path| format!("const HOOK: string = {};\n", serde_json::json!(p));
        std::fs::write(&ext, source(&hook)).unwrap();
        assert_eq!(check_pi_extension(&ext).state, State::Ok);

        // The app moved or was deleted: the extension still loads, and every call fails open.
        std::fs::write(&ext, source(&dir.path().join("gone"))).unwrap();
        let check = check_pi_extension(&ext);
        assert_eq!(check.state, State::Broken);
        assert!(check.detail.contains("gone"), "{}", check.detail);
    }

    #[test]
    fn hermes_config_is_checked_for_both_events_and_the_right_flag() {
        let ok = "model: hermes-4\nmcp_servers:\n  logbear:\n    command: \"/x/logbear-mcp\"\n\
                  hooks:\n  pre_llm_call:\n    - command: \"/x/logbear-hook --harness hermes\"\n\
                  \x20 pre_verify:\n    - command: \"/x/logbear-hook --harness hermes\"\n";
        let [mcp, hooks] = check_hermes_config(Some(ok));
        assert_eq!(
            (mcp.state, hooks.state),
            (State::Ok, State::Ok),
            "{}",
            hooks.detail
        );

        let half = "hooks:\n  pre_verify:\n    - command: \"/x/logbear-hook --harness hermes\"\n";
        let [mcp, hooks] = check_hermes_config(Some(half));
        assert_eq!(mcp.state, State::Missing);
        assert_eq!(hooks.state, State::Broken, "{}", hooks.detail);
        assert!(hooks.detail.contains("pre_llm_call"), "{}", hooks.detail);

        let wrong = ok.replace("--harness hermes", "--harness claude");
        assert_eq!(check_hermes_config(Some(&wrong))[1].state, State::Broken);

        let [mcp, hooks] = check_hermes_config(None);
        assert_eq!((mcp.state, hooks.state), (State::Missing, State::Missing));
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

//! The installer.
//!
//! The app *is* the installer: one download, no `cargo install`, no hand-edited JSON. Four
//! things get placed, each with explicit confirmation from the user and each individually
//! reversible.
//!
//! The rule that matters most: **config writes merge, never overwrite.** These are files
//! people have spent real time on. Every write is preceded by a timestamped backup, every
//! step reports what it would change before it changes it, and uninstall puts things back.
//! Clobbering someone's `settings.json` is exactly the bad experience the design forbids.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Symlink `runlog` onto PATH.
    Path,
    /// Register `runlogd` as a user-level launch agent / systemd user service.
    Daemon,
    McpClaude,
    McpCodex,
    HookClaude,
    HookCodex,
}

impl Step {
    pub fn parse(s: &str) -> Option<Step> {
        Some(match s {
            "path" => Step::Path,
            "daemon" => Step::Daemon,
            "mcp_claude" => Step::McpClaude,
            "mcp_codex" => Step::McpCodex,
            "hook_claude" => Step::HookClaude,
            "hook_codex" => Step::HookCodex,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepState {
    pub installed: bool,
    /// False when the target harness is not on this machine, so the UI can say
    /// "Codex not detected" instead of offering a button that cannot work.
    pub available: bool,
    pub detail: String,
    pub backup: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallStatus {
    pub path: StepState,
    pub daemon: StepState,
    pub mcp_claude: StepState,
    pub mcp_codex: StepState,
    pub hook_claude: StepState,
    pub hook_codex: StepState,
}

pub fn status() -> InstallStatus {
    InstallStatus {
        path: path_status(),
        daemon: daemon_status(),
        mcp_claude: mcp_status(Harness::Claude),
        mcp_codex: mcp_status(Harness::Codex),
        hook_claude: hook_status(Harness::Claude),
        hook_codex: hook_status(Harness::Codex),
    }
}

pub fn install(step: Step) -> Result<StepState, String> {
    match step {
        Step::Path => install_path(),
        Step::Daemon => install_daemon(),
        Step::McpClaude => install_mcp(Harness::Claude),
        Step::McpCodex => install_mcp(Harness::Codex),
        Step::HookClaude => install_hook(Harness::Claude),
        Step::HookCodex => install_hook(Harness::Codex),
    }
}

pub fn uninstall(step: Step) -> Result<StepState, String> {
    match step {
        Step::Path => uninstall_path(),
        Step::Daemon => uninstall_daemon(),
        Step::McpClaude => uninstall_mcp(Harness::Claude),
        Step::McpCodex => uninstall_mcp(Harness::Codex),
        Step::HookClaude => uninstall_hook(Harness::Claude),
        Step::HookCodex => uninstall_hook(Harness::Codex),
    }
}

// ---------------------------------------------------------------------------
// Where our binaries are
// ---------------------------------------------------------------------------

/// Binaries ship beside the app executable, as Tauri sidecars.
fn bin_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn bin(name: &str) -> PathBuf {
    bin_dir().join(name)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

// ---------------------------------------------------------------------------
// PATH
// ---------------------------------------------------------------------------

/// `/usr/local/bin` needs admin rights on a stock macOS, and asking for them during a
/// first launch is a bad trade. `~/.local/bin` is on PATH for most developers and is
/// writable without a prompt, so it is tried first.
fn path_targets() -> Vec<PathBuf> {
    vec![home().join(".local/bin"), PathBuf::from("/usr/local/bin")]
}

fn path_status() -> StepState {
    for dir in path_targets() {
        let link = dir.join("runlog");
        if let Ok(target) = std::fs::read_link(&link) {
            if target == bin("runlog") {
                return StepState {
                    installed: true,
                    available: true,
                    detail: format!("{} → {}", link.display(), target.display()),
                    backup: None,
                };
            }
            return StepState {
                installed: false,
                available: true,
                detail: format!(
                    "{} points at a different runlog ({}). Installing will repoint it.",
                    link.display(),
                    target.display()
                ),
                backup: None,
            };
        }
        if link.exists() {
            return StepState {
                installed: false,
                available: true,
                detail: format!("{} exists and is not a symlink; it will be left alone", link.display()),
                backup: None,
            };
        }
    }
    StepState {
        installed: false,
        available: true,
        detail: "runlog is not on your PATH".into(),
        backup: None,
    }
}

fn install_path() -> Result<StepState, String> {
    let source = bin("runlog");
    if !source.exists() {
        return Err(format!("the runlog binary is missing from {}", bin_dir().display()));
    }
    let mut last_error = String::new();
    for dir in path_targets() {
        if std::fs::create_dir_all(&dir).is_err() {
            last_error = format!("cannot create {}", dir.display());
            continue;
        }
        let link = dir.join("runlog");
        // Replace only a symlink. A real file there belongs to someone else.
        if link.is_symlink() {
            let _ = std::fs::remove_file(&link);
        } else if link.exists() {
            last_error = format!("{} already exists and is not a symlink", link.display());
            continue;
        }
        match std::os::unix::fs::symlink(&source, &link) {
            Ok(()) => {
                let on_path = std::env::var_os("PATH")
                    .map(|p| std::env::split_paths(&p).any(|d| d == dir))
                    .unwrap_or(false);
                return Ok(StepState {
                    installed: true,
                    available: true,
                    detail: if on_path {
                        format!("linked into {}", dir.display())
                    } else {
                        format!(
                            "linked into {}, which is not on your PATH — add it to your shell profile",
                            dir.display()
                        )
                    },
                    backup: None,
                });
            }
            Err(e) => last_error = format!("{}: {e}", link.display()),
        }
    }
    Err(last_error)
}

fn uninstall_path() -> Result<StepState, String> {
    for dir in path_targets() {
        let link = dir.join("runlog");
        if link.is_symlink() {
            let _ = std::fs::remove_file(&link);
        }
    }
    Ok(path_status())
}

// ---------------------------------------------------------------------------
// Daemon service
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn service_file() -> PathBuf {
    home().join("Library/LaunchAgents/sh.runlog.daemon.plist")
}

#[cfg(target_os = "linux")]
fn service_file() -> PathBuf {
    home().join(".config/systemd/user/runlogd.service")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn service_file() -> PathBuf {
    home().join(".runlog/runlogd.service")
}

fn daemon_status() -> StepState {
    let running = runlog_core::client::Client::connect().is_ok();
    let registered = service_file().exists();
    StepState {
        installed: registered,
        available: true,
        detail: match (registered, running) {
            (true, true) => "registered and running".into(),
            (true, false) => "registered but not running".into(),
            (false, true) => "running, but will not restart after a reboot".into(),
            (false, false) => "not registered".into(),
        },
        backup: None,
    }
}

fn install_daemon() -> Result<StepState, String> {
    let exe = bin("runlogd");
    if !exe.exists() {
        return Err(format!("runlogd is missing from {}", bin_dir().display()));
    }
    let path = service_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let log = runlog_core::paths::daemon_log();

    #[cfg(target_os = "macos")]
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>sh.runlog.daemon</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>ProcessType</key><string>Background</string>
</dict>
</plist>
"#,
        exe = exe.display(),
        log = log.display()
    );

    #[cfg(not(target_os = "macos"))]
    let body = format!(
        "[Unit]\nDescription=runlog daemon\n\n\
         [Service]\nExecStart={exe} serve\nRestart=always\nRestartSec=2\n\n\
         [Install]\nWantedBy=default.target\n",
        exe = exe.display()
    );

    std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
    load_service(&path);
    Ok(daemon_status())
}

fn uninstall_daemon() -> Result<StepState, String> {
    let path = service_file();
    unload_service(&path);
    let _ = std::fs::remove_file(&path);
    Ok(daemon_status())
}

#[cfg(target_os = "macos")]
fn load_service(path: &Path) {
    use std::process::Command;
    let uid = format!("gui/{}", unsafe { libc_getuid() });
    let _ = Command::new("launchctl").args(["bootout", &uid, &path.to_string_lossy()]).status();
    let _ = Command::new("launchctl").args(["bootstrap", &uid, &path.to_string_lossy()]).status();
}

#[cfg(target_os = "macos")]
fn unload_service(path: &Path) {
    use std::process::Command;
    let uid = format!("gui/{}", unsafe { libc_getuid() });
    let _ = Command::new("launchctl").args(["bootout", &uid, &path.to_string_lossy()]).status();
}

#[cfg(target_os = "macos")]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

#[cfg(not(target_os = "macos"))]
fn load_service(_path: &Path) {
    use std::process::Command;
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    let _ = Command::new("systemctl").args(["--user", "enable", "--now", "runlogd"]).status();
}

#[cfg(not(target_os = "macos"))]
fn unload_service(_path: &Path) {
    use std::process::Command;
    let _ = Command::new("systemctl").args(["--user", "disable", "--now", "runlogd"]).status();
}

// ---------------------------------------------------------------------------
// Harness config
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
}

impl Harness {
    fn config(self) -> PathBuf {
        match self {
            Harness::Claude => home().join(".claude/settings.json"),
            Harness::Codex => home().join(".codex/hooks.json"),
        }
    }

    fn mcp_config(self) -> PathBuf {
        match self {
            Harness::Claude => home().join(".claude.json"),
            Harness::Codex => home().join(".codex/config.toml"),
        }
    }

    fn present(self) -> bool {
        let dir = match self {
            Harness::Claude => home().join(".claude"),
            Harness::Codex => home().join(".codex"),
        };
        dir.exists()
    }

    fn label(self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::Codex => "Codex",
        }
    }
}

fn mcp_status(h: Harness) -> StepState {
    let path = h.mcp_config();
    let installed = match h {
        Harness::Claude => read_json(&path)
            .and_then(|v| v.get("mcpServers").cloned())
            .map(|v| v.get("runlog").is_some())
            .unwrap_or(false),
        Harness::Codex => std::fs::read_to_string(&path)
            .map(|s| s.contains("[mcp_servers.runlog]"))
            .unwrap_or(false),
    };
    StepState {
        installed,
        available: h.present(),
        detail: if !h.present() {
            format!("{} was not detected on this machine", h.label())
        } else if installed {
            format!("registered in {}", path.display())
        } else {
            format!("not registered in {}", path.display())
        },
        backup: None,
    }
}

fn install_mcp(h: Harness) -> Result<StepState, String> {
    let server = bin("runlog-mcp");
    if !server.exists() {
        return Err(format!("runlog-mcp is missing from {}", bin_dir().display()));
    }
    let path = h.mcp_config();
    let backup = back_up(&path)?;

    match h {
        Harness::Claude => {
            let mut root = read_json(&path).unwrap_or_else(|| serde_json::json!({}));
            if !root.is_object() {
                return Err(format!("{} is not a JSON object; leaving it alone", path.display()));
            }
            let servers = root
                .as_object_mut()
                .unwrap()
                .entry("mcpServers")
                .or_insert_with(|| serde_json::json!({}));
            if !servers.is_object() {
                return Err(format!("mcpServers in {} is not an object", path.display()));
            }
            servers.as_object_mut().unwrap().insert(
                "runlog".into(),
                serde_json::json!({
                    "command": server.to_string_lossy(),
                    "args": [],
                }),
            );
            write_json(&path, &root)?;
        }
        Harness::Codex => {
            // TOML, and the file may hold anything. Append a clearly delimited block
            // rather than parsing and rewriting someone else's config.
            let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
            if !existing.contains("[mcp_servers.runlog]") {
                if !existing.is_empty() && !existing.ends_with('\n') {
                    existing.push('\n');
                }
                existing.push_str(&format!(
                    "\n# added by runlog\n[mcp_servers.runlog]\ncommand = \"{}\"\nargs = []\n",
                    server.to_string_lossy()
                ));
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&path, existing).map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
    }

    let mut state = mcp_status(h);
    state.backup = backup;
    Ok(state)
}

fn uninstall_mcp(h: Harness) -> Result<StepState, String> {
    let path = h.mcp_config();
    match h {
        Harness::Claude => {
            if let Some(mut root) = read_json(&path) {
                if let Some(servers) = root.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
                    servers.remove("runlog");
                    write_json(&path, &root)?;
                }
            }
        }
        Harness::Codex => {
            if let Ok(existing) = std::fs::read_to_string(&path) {
                let cleaned = strip_toml_block(&existing, "[mcp_servers.runlog]");
                std::fs::write(&path, cleaned).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(mcp_status(h))
}

fn hook_status(h: Harness) -> StepState {
    let path = h.config();
    let installed = std::fs::read_to_string(&path)
        .map(|s| s.contains("runlog-hook"))
        .unwrap_or(false);
    StepState {
        installed,
        available: h.present(),
        detail: if !h.present() {
            format!("{} was not detected on this machine", h.label())
        } else if installed {
            format!("installed in {}", path.display())
        } else {
            format!("not installed in {}", path.display())
        },
        backup: None,
    }
}

/// The hook binary knows its own config shape, so it emits the block to merge and this
/// only has to merge it. One source of truth for a format two vendors keep changing.
fn hook_block(h: Harness) -> Result<serde_json::Value, String> {
    let exe = bin("runlog-hook");
    if !exe.exists() {
        return Err(format!("runlog-hook is missing from {}", bin_dir().display()));
    }
    let flag = match h {
        Harness::Claude => "claude",
        Harness::Codex => "codex",
    };
    let out = std::process::Command::new(&exe)
        .args(["--emit-config", flag])
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("runlog-hook emitted invalid JSON: {e}"))
}

fn install_hook(h: Harness) -> Result<StepState, String> {
    let block = hook_block(h)?;
    let path = h.config();
    let backup = back_up(&path)?;

    let mut root = read_json(&path).unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        return Err(format!("{} is not a JSON object; leaving it alone", path.display()));
    }
    merge(&mut root, &block);
    write_json(&path, &root)?;

    let mut state = hook_status(h);
    state.backup = backup;
    Ok(state)
}

fn uninstall_hook(h: Harness) -> Result<StepState, String> {
    let path = h.config();
    let Some(mut root) = read_json(&path) else {
        return Ok(hook_status(h));
    };
    prune_runlog(&mut root);
    write_json(&path, &root)?;
    Ok(hook_status(h))
}

// ---------------------------------------------------------------------------
// JSON merging
// ---------------------------------------------------------------------------

/// Deep merge, additive for arrays and recursive for objects.
///
/// Never replaces a container the user owns. A `hooks.Stop` array that already has three
/// of their entries keeps all three and gains ours.
fn merge(target: &mut serde_json::Value, patch: &serde_json::Value) {
    match (target, patch) {
        (serde_json::Value::Object(t), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                match t.get_mut(k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        t.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (serde_json::Value::Array(t), serde_json::Value::Array(p)) => {
            for v in p {
                if !t.contains(v) {
                    t.push(v.clone());
                }
            }
        }
        (t, p) => *t = p.clone(),
    }
}

/// Remove anything mentioning `runlog-hook`, then drop containers left empty by it, so
/// uninstall does not leave a litter of empty arrays behind.
fn prune_runlog(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                if let Some(v) = map.get_mut(&k) {
                    if mentions_runlog(v) && !v.is_object() && !v.is_array() {
                        map.remove(&k);
                        continue;
                    }
                    prune_runlog(v);
                    let empty = match v {
                        serde_json::Value::Array(a) => a.is_empty(),
                        serde_json::Value::Object(o) => o.is_empty(),
                        _ => false,
                    };
                    if empty {
                        map.remove(&k);
                    }
                }
            }
        }
        serde_json::Value::Array(items) => {
            items.retain(|v| !mentions_runlog(v));
            for v in items.iter_mut() {
                prune_runlog(v);
            }
        }
        _ => {}
    }
}

fn mentions_runlog(v: &serde_json::Value) -> bool {
    v.to_string().contains("runlog-hook")
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(path, body + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

/// Copy a config aside before touching it. Returns the backup path, which the UI shows
/// so "revert" is a real offer rather than a promise.
fn back_up(path: &Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = runlog_core::now_micros();
    let backup = path.with_extension(format!("runlog-backup.{stamp}"));
    std::fs::copy(path, &backup).map_err(|e| format!("could not back up {}: {e}", path.display()))?;
    Ok(Some(backup.to_string_lossy().into_owned()))
}

/// Remove a `[header]` section and its body from a TOML file, up to the next section.
fn strip_toml_block(source: &str, header: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed == header {
            skipping = true;
            // Also drop the "# added by runlog" marker we wrote above it.
            if out.last().map(|l: &&str| l.trim() == "# added by runlog").unwrap_or(false) {
                out.pop();
            }
            continue;
        }
        if skipping {
            if trimmed.starts_with('[') {
                skipping = false;
            } else {
                continue;
            }
        }
        out.push(line);
    }
    let mut s = out.join("\n");
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merging_preserves_everything_the_user_already_had() {
        let mut existing = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{ "hooks": [{ "type": "command", "command": "their-tool" }] }],
                "PreToolUse": [{ "hooks": [{ "type": "command", "command": "other" }] }]
            }
        });
        let ours = json!({
            "hooks": {
                "Stop": [{ "hooks": [{ "type": "command", "command": "runlog-hook stop" }] }]
            }
        });
        merge(&mut existing, &ours);

        assert_eq!(existing["model"], "opus", "an unrelated key was lost");
        let stop = existing["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "their Stop hook was replaced instead of joined");
        assert!(existing["hooks"]["PreToolUse"].is_array(), "an unrelated hook was lost");
    }

    #[test]
    fn merging_twice_does_not_duplicate_our_entry() {
        let mut existing = json!({});
        let ours = json!({ "hooks": { "Stop": [{ "command": "runlog-hook" }] } });
        merge(&mut existing, &ours);
        merge(&mut existing, &ours);
        assert_eq!(existing["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn uninstall_removes_only_our_entry() {
        let mut config = json!({
            "model": "opus",
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "their-tool" }] },
                    { "hooks": [{ "type": "command", "command": "/opt/runlog-hook stop" }] }
                ]
            }
        });
        prune_runlog(&mut config);
        assert_eq!(config["model"], "opus");
        let stop = config["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(stop[0].to_string().contains("their-tool"));
    }

    #[test]
    fn uninstall_leaves_no_empty_containers_behind() {
        let mut config = json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": "runlog-hook" }] }] },
            "model": "opus"
        });
        prune_runlog(&mut config);
        assert!(config.get("hooks").is_none(), "an empty hooks object was left: {config}");
        assert_eq!(config["model"], "opus");
    }

    #[test]
    fn a_toml_block_is_removed_without_disturbing_its_neighbours() {
        let source = "[general]\nfoo = 1\n\n# added by runlog\n[mcp_servers.runlog]\ncommand = \"/x\"\nargs = []\n\n[mcp_servers.other]\ncommand = \"/y\"\n";
        let out = strip_toml_block(source, "[mcp_servers.runlog]");
        assert!(!out.contains("mcp_servers.runlog"));
        assert!(!out.contains("added by runlog"));
        assert!(out.contains("[general]"), "{out}");
        assert!(out.contains("[mcp_servers.other]"), "{out}");
        assert!(out.contains("command = \"/y\""), "{out}");
    }

    #[test]
    fn a_config_that_is_not_an_object_is_refused_rather_than_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "[\"a list, somehow\"]").unwrap();
        let root = read_json(&path).unwrap();
        assert!(!root.is_object(), "the guard in install_hook depends on this");
    }

    #[test]
    fn backing_up_a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(back_up(&dir.path().join("nope.json")).unwrap(), None);
    }

    #[test]
    fn backing_up_copies_the_original_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\"model\":\"opus\"}").unwrap();
        let backup = back_up(&path).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "{\"model\":\"opus\"}");
    }

    #[test]
    fn step_names_match_what_the_ui_sends() {
        for name in ["path", "daemon", "mcp_claude", "mcp_codex", "hook_claude", "hook_codex"] {
            assert!(Step::parse(name).is_some(), "unknown step: {name}");
        }
        assert!(Step::parse("nonsense").is_none());
    }
}

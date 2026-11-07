//! `logbear updates` and `logbear upgrade` — the only network requests logbear can make.
//!
//! Checks are off by default. When turned on, at most once a day, `logbear doctor` asks
//! GitHub for the latest release: one HTTPS GET to the releases API, carrying nothing but a
//! `User-Agent` with the installed version. `logbear upgrade` makes the same request when
//! you run it, then hands over to whichever installer put this copy here. Nothing about
//! projects, runs or output is ever sent. It goes through the system `curl` rather than an
//! HTTP client compiled into every binary, so the whole of logbear's network code is this
//! file.

use crate::ui;
use logbear_core::paths;
use logbear_core::client::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

const RELEASES: &str = "https://api.github.com/repos/teralabx/logbear/releases/latest";
const DAY_MICROS: i64 = 24 * 60 * 60 * 1_000_000;
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const FORMULA: &str = "teralabx/tap/logbear";
const DOWNLOADS: &str = "https://github.com/teralabx/logbear/releases";

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
    paths::write_private(&file(), (body + "\n").as_bytes())
        .map_err(|e| format!("{}: {e}", file().display()))
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

/// How this copy got here, which decides what can replace it.
#[derive(Debug, PartialEq)]
enum Channel {
    Homebrew,
    /// `install.sh`, into this directory.
    Script(PathBuf),
    /// Inside the desktop app, which is replaced as a whole.
    App,
    /// Built from source: `cargo install`, or a checkout's `target` directory.
    Source,
}

fn channel() -> Channel {
    let exe = std::env::current_exe().unwrap_or_default();
    let resolved = exe.canonicalize().unwrap_or(exe);
    classify(&resolved, logbear_setup::homebrew_managed())
}

/// `exe` is resolved: a PATH symlink into the app has to count as the app.
fn classify(exe: &Path, homebrew: bool) -> Channel {
    let text = exe.to_string_lossy();
    if homebrew || text.contains("/Cellar/") {
        Channel::Homebrew
    } else if text.contains(".app/Contents/MacOS") {
        Channel::App
    } else if text.contains("/.cargo/bin/") || text.contains("/target/") {
        Channel::Source
    } else {
        Channel::Script(exe.parent().unwrap_or(Path::new(".")).to_path_buf())
    }
}

fn upgrade_hint() -> String {
    hint_for(&channel())
}

fn hint_for(channel: &Channel) -> String {
    match channel {
        Channel::Homebrew | Channel::Script(_) => "run `logbear upgrade`".into(),
        Channel::App => format!("download the new app from {DOWNLOADS}"),
        Channel::Source => "this copy was built from source; rebuild it to upgrade".into(),
    }
}

/// `logbear upgrade`: asked for by name, so it reaches GitHub whether or not checks are on.
pub fn upgrade() -> Result<i32, String> {
    let channel = channel();
    let mut state = load();
    check_now(&mut state).map_err(|e| format!("logbear upgrade: could not reach GitHub ({e})"))?;
    save(&state)?;
    let latest = state.latest.clone().unwrap_or_default();

    if !newer(&latest, CURRENT) {
        println!("logbear v{CURRENT} is the latest");
        return Ok(0);
    }
    println!("Upgrading logbear v{CURRENT} to v{latest}");

    let dir = logbear_core::paths::install_dir(&std::env::current_exe().unwrap_or_default());
    match &channel {
        Channel::Homebrew => upgrade_with_brew()?,
        Channel::Script(dir) => upgrade_with_script(&latest, dir)?,
        Channel::App | Channel::Source => {
            return Err(format!("logbear upgrade: {}", hint_for(&channel)))
        }
    }

    // A tap that has not caught up leaves brew happily reporting nothing to do, so believe
    // the binary rather than the installer's exit code.
    let installed = installed_version(&channel, &dir);
    if installed.as_deref() != Some(latest.as_str()) {
        return Err(format!(
            "logbear upgrade: expected v{latest} but found {}",
            installed.map_or("no binary".into(), |v| format!("v{v}"))
        ));
    }
    println!("logbear is now v{latest}");
    if let Ok(status) = Client::connect().and_then(|mut c| c.status()) {
        println!("{}", daemon_note(&status.version, status.active_runs));
    }
    Ok(0)
}

/// Explicitly first: `brew upgrade` only refreshes the tap once a day, so it could miss a
/// release this command has just seen.
fn upgrade_with_brew() -> Result<(), String> {
    execute(Command::new("brew").args(["update", "--quiet"]))?;
    execute(Command::new("brew").args(["upgrade", FORMULA]))
}

/// The installer from the release's own tag, so it matches the archive it is about to fetch.
fn upgrade_with_script(version: &str, dir: &Path) -> Result<(), String> {
    let url = std::env::var("LOGBEAR_INSTALLER_URL").unwrap_or_else(|_| {
        format!("https://raw.githubusercontent.com/teralabx/logbear/v{version}/install.sh")
    });
    let script = std::env::temp_dir().join(format!("logbear-install-{}.sh", std::process::id()));
    let result = execute(Command::new("curl").args(["-fsSL", "--max-time", "30", "-o"]).arg(&script).arg(&url))
        .and_then(|()| {
            execute(Command::new("sh")
                .arg(&script)
                .env("LOGBEAR_VERSION", version)
                .env("LOGBEAR_INSTALL_DIR", dir)
                .env("LOGBEAR_UPGRADING", "1"))
        });
    let _ = std::fs::remove_file(&script);
    result
}

fn execute(cmd: &mut Command) -> Result<(), String> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let status = cmd.status().map_err(|e| format!("logbear upgrade: {name}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("logbear upgrade: {name} failed ({status})"))
    }
}

fn installed_version(channel: &Channel, stable: &Path) -> Option<String> {
    let dir = match channel {
        Channel::Script(dir) => dir,
        _ => stable,
    };
    let out = Command::new(dir.join("logbear")).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().last().map(str::to_string)
}

fn daemon_note(daemon: &str, active_runs: usize) -> String {
    match active_runs {
        0 => format!("The daemon is still v{daemon}; it restarts itself in a few seconds."),
        n => format!(
            "The daemon is still v{daemon}; it restarts itself once {n} live run{} finish{}.",
            if n == 1 { "" } else { "s" },
            if n == 1 { "es" } else { "" }
        ),
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
    fn each_install_is_upgraded_by_what_installed_it() {
        let classified = |p: &str, brew| classify(Path::new(p), brew);
        assert_eq!(classified("/opt/homebrew/opt/logbear/bin/logbear", true), Channel::Homebrew);
        assert_eq!(
            classified("/opt/homebrew/Cellar/logbear/0.1.0/bin/logbear", false),
            Channel::Homebrew
        );
        assert_eq!(
            classified("/Applications/logbear.app/Contents/MacOS/logbear", false),
            Channel::App
        );
        assert_eq!(classified("/Users/a/.cargo/bin/logbear", false), Channel::Source);
        assert_eq!(classified("/Users/a/src/logbear/target/release/logbear", false), Channel::Source);
        assert_eq!(
            classified("/Users/a/.local/bin/logbear", false),
            Channel::Script(PathBuf::from("/Users/a/.local/bin"))
        );
    }

    #[test]
    fn only_installs_logbear_can_replace_are_told_to_upgrade_themselves() {
        assert!(hint_for(&Channel::Homebrew).contains("logbear upgrade"));
        assert!(hint_for(&Channel::Script(PathBuf::from("/x"))).contains("logbear upgrade"));
        assert!(hint_for(&Channel::App).contains("download"));
        assert!(!hint_for(&Channel::Source).contains("logbear upgrade"));
    }

    #[test]
    fn the_daemon_note_says_what_it_is_waiting_for() {
        assert!(daemon_note("0.1.0", 0).contains("few seconds"));
        assert!(daemon_note("0.1.0", 1).contains("1 live run finishes"));
        assert!(daemon_note("0.1.0", 2).contains("2 live runs finish."));
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

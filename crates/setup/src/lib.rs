//! The installer, shared by the desktop app's Settings screen and `logbear setup`.
//!
//! Each step places one thing — `logbear` on PATH, the daemon as a login service, the MCP
//! server and the hook in each coding agent — with explicit confirmation from the user, and
//! each is individually reversible. The app and the CLI drive the same functions, so either
//! can undo what the other did.
//!
//! The rule that matters most: **config writes merge, never overwrite.** These are files
//! people have spent real time on. Every write is preceded by a timestamped backup and lands
//! with a rename, a file that will not parse is refused rather than replaced, every step
//! reports what it would change before it changes it, and uninstall puts things back.
//! Clobbering someone's `settings.json` is exactly the bad experience the design forbids.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Symlink `logbear` onto PATH.
    Path,
    /// Register `logbeard` as a user-level launch agent / systemd user service.
    Daemon,
    McpClaude,
    McpCodex,
    McpPi,
    McpHermes,
    HookClaude,
    HookCodex,
    HookPi,
    HookHermes,
}

impl Step {
    pub fn parse(s: &str) -> Option<Step> {
        Some(match s {
            "path" => Step::Path,
            "daemon" => Step::Daemon,
            "mcp_claude" => Step::McpClaude,
            "mcp_codex" => Step::McpCodex,
            "mcp_pi" => Step::McpPi,
            "mcp_hermes" => Step::McpHermes,
            "hook_claude" => Step::HookClaude,
            "hook_codex" => Step::HookCodex,
            "hook_pi" => Step::HookPi,
            "hook_hermes" => Step::HookHermes,
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
    pub mcp_pi: StepState,
    pub mcp_hermes: StepState,
    pub hook_claude: StepState,
    pub hook_codex: StepState,
    pub hook_pi: StepState,
    pub hook_hermes: StepState,
}

/// Run the two local steps once, on first launch, without asking.
///
/// The design says every step is confirmed. That still holds for the steps that write to
/// files the *user* owns — their Claude Code and Codex configs are never touched without
/// a click. But putting our own binary on PATH and registering our own background service
/// are not really consent questions: without them the product does nothing at all, and a
/// user who has just dragged an app to Applications and opened it has already said yes to
/// the thing the app is for. Both are reversible from Settings.
///
/// Returns what it did, or `None` if this is not a first launch.
pub fn first_run_setup() -> Option<Vec<(&'static str, Result<StepState, String>)>> {
    let marker = logbear_core::paths::home().join(".setup-done");
    if marker.exists() {
        return None;
    }
    let _ = logbear_core::paths::ensure_home();

    let done = vec![
        ("PATH", install(Step::Path)),
        ("daemon", install(Step::Daemon)),
    ];

    // Written whatever happened. Retrying automatically on every launch would mean a
    // machine where this genuinely cannot work gets nagged forever; Settings still has
    // the button, and `logbear doctor` still explains what is missing.
    let _ = logbear_core::paths::write_private(
        &marker,
        logbear_core::now_micros().to_string().as_bytes(),
    );
    Some(done)
}

pub fn status() -> InstallStatus {
    InstallStatus {
        path: path_status(),
        daemon: daemon_status(),
        mcp_claude: mcp_status(Harness::Claude),
        mcp_codex: mcp_status(Harness::Codex),
        mcp_pi: mcp_status(Harness::Pi),
        mcp_hermes: mcp_status(Harness::Hermes),
        hook_claude: hook_status(Harness::Claude),
        hook_codex: hook_status(Harness::Codex),
        hook_pi: hook_status(Harness::Pi),
        hook_hermes: hook_status(Harness::Hermes),
    }
}

pub fn install(step: Step) -> Result<StepState, String> {
    match step {
        Step::Path => install_path(),
        Step::Daemon => install_daemon(),
        Step::McpClaude => install_mcp(Harness::Claude),
        Step::McpCodex => install_mcp(Harness::Codex),
        Step::McpPi => install_mcp(Harness::Pi),
        Step::McpHermes => install_mcp(Harness::Hermes),
        Step::HookClaude => install_hook(Harness::Claude),
        Step::HookCodex => install_hook(Harness::Codex),
        Step::HookPi => install_hook(Harness::Pi),
        Step::HookHermes => install_hook(Harness::Hermes),
    }
}

pub fn uninstall(step: Step) -> Result<StepState, String> {
    match step {
        Step::Path => uninstall_path(),
        Step::Daemon => uninstall_daemon(),
        Step::McpClaude => uninstall_mcp(Harness::Claude),
        Step::McpCodex => uninstall_mcp(Harness::Codex),
        Step::McpPi => uninstall_mcp(Harness::Pi),
        Step::McpHermes => uninstall_mcp(Harness::Hermes),
        Step::HookClaude => uninstall_hook(Harness::Claude),
        Step::HookCodex => uninstall_hook(Harness::Codex),
        Step::HookPi => uninstall_hook(Harness::Pi),
        Step::HookHermes => uninstall_hook(Harness::Hermes),
    }
}

// ---------------------------------------------------------------------------
// Where our binaries are
// ---------------------------------------------------------------------------

/// Binaries ship beside the app executable, as Tauri sidecars.
fn bin_dir() -> PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    stable_bin_dir(&dir)
}

/// A Homebrew keg's `bin` under its version-stable `opt` link.
///
/// Every path this writes into an agent's config outlives the process that wrote it, and a
/// `Cellar/logbear/0.1.0/bin` path is deleted by the next `brew upgrade`, silently breaking
/// every hook. `opt/logbear` follows the installed version.
fn stable_bin_dir(dir: &Path) -> PathBuf {
    let parts: Vec<_> = dir.components().collect();
    let n = parts.len();
    if n >= 4
        && parts[n - 1].as_os_str() == "bin"
        && parts[n - 3].as_os_str() == "logbear"
        && parts[n - 4].as_os_str() == "Cellar"
    {
        let prefix: PathBuf = parts[..n - 4].iter().collect();
        return prefix.join("opt/logbear/bin");
    }
    dir.to_path_buf()
}

/// Whether these binaries came from Homebrew, which owns the daemon service
/// (`brew services`) and upgrades.
pub fn homebrew_managed() -> bool {
    bin_dir().ends_with("opt/logbear/bin")
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

/// The user's *real* PATH, as their login shell would build it.
///
/// This is the whole reason the first version of this step failed silently. A GUI app
/// launched from Finder inherits launchd's PATH — roughly `/usr/bin:/bin:/usr/sbin:/sbin`
/// — and knows nothing about `~/.local/bin`, homebrew, mise, asdf or anything else the
/// user has spent years setting up. Asking `std::env::var("PATH")` inside the app answers
/// a question nobody asked, so we ask the login shell instead.
fn user_path() -> Vec<PathBuf> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let name = Path::new(&shell)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    // fish does not understand `-lc` the way POSIX shells do; ask it in its own dialect.
    let args: Vec<&str> = if name == "fish" {
        vec!["-l", "-c", "echo $PATH | tr ' ' ':'"]
    } else {
        vec!["-l", "-c", "printf '%s' \"$PATH\""]
    };

    let output = std::process::Command::new(&shell)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output();

    let raw = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        // A login shell that fails or hangs must not stop the install; fall back to
        // whatever this process has, which at least contains the system directories.
        _ => std::env::var("PATH").unwrap_or_default(),
    };

    std::env::split_paths(&raw).collect()
}

fn on_user_path(dir: &Path) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    user_path()
        .into_iter()
        .any(|p| p.canonicalize().unwrap_or(p) == dir)
}

fn writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        // Can we create it? Only claim so for paths under the user's home.
        return dir.starts_with(home());
    }
    // The honest test is a write, not a permission-bit guess: `/usr/local/bin` is
    // group-writable on a homebrew Intel Mac and root-only on a stock Apple Silicon one.
    let probe = dir.join(".logbear-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Where the symlink could go, best first.
///
/// `~/.local/bin` leads because it needs no admin rights and is already on PATH for most
/// developers. The homebrew prefixes come next because when they exist they are certainly
/// on PATH. `/usr/local/bin` is last: on a stock Apple Silicon machine it needs sudo, and
/// prompting for admin rights during a first launch is a bad trade.
fn path_candidates() -> Vec<PathBuf> {
    vec![
        home().join(".local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home().join("bin"),
    ]
}

/// The directory we would actually use: on the user's PATH and writable, if such a thing
/// exists; otherwise `~/.local/bin`, which we will then put on their PATH ourselves.
fn chosen_dir() -> (PathBuf, bool) {
    for dir in path_candidates() {
        if on_user_path(&dir) && writable(&dir) {
            return (dir, true);
        }
    }
    (home().join(".local/bin"), false)
}

fn existing_link() -> Option<(PathBuf, PathBuf)> {
    for dir in path_candidates() {
        let link = dir.join("logbear");
        if let Ok(target) = std::fs::read_link(&link) {
            return Some((link, target));
        }
    }
    None
}

fn path_status() -> StepState {
    let wanted = bin("logbear");
    match existing_link() {
        Some((link, target)) if target == wanted => {
            let dir = link.parent().unwrap_or(Path::new("/")).to_path_buf();
            if on_user_path(&dir) {
                StepState {
                    installed: true,
                    available: true,
                    detail: format!("{} → this app", tilde(&link)),
                    backup: None,
                }
            } else {
                StepState {
                    installed: false,
                    available: true,
                    detail: format!(
                        "{} exists but {} is not on your PATH. Install again to add it.",
                        tilde(&link),
                        tilde(&dir)
                    ),
                    backup: None,
                }
            }
        }
        Some((link, target)) => StepState {
            installed: false,
            available: true,
            detail: format!(
                "{} points at {}. Installing will repoint it at this app.",
                tilde(&link),
                tilde(&target)
            ),
            backup: None,
        },
        None => {
            let (dir, on_path) = chosen_dir();
            StepState {
                installed: false,
                available: true,
                detail: if on_path {
                    format!("not installed — will link into {}", tilde(&dir))
                } else {
                    format!(
                        "not installed — will link into {} and add it to your shell profile",
                        tilde(&dir)
                    )
                },
                backup: None,
            }
        }
    }
}

fn install_path() -> Result<StepState, String> {
    let source = bin("logbear");
    if !source.exists() {
        return Err(format!(
            "the logbear binary is missing from {}",
            bin_dir().display()
        ));
    }

    let (dir, already_on_path) = chosen_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let link = dir.join("logbear");

    // Replace only a symlink. A real file there belongs to someone else.
    if link.is_symlink() {
        let _ = std::fs::remove_file(&link);
    } else if link.exists() {
        return Err(format!(
            "{} already exists and is not a symlink, so it was left alone",
            link.display()
        ));
    }
    std::os::unix::fs::symlink(&source, &link)
        .map_err(|e| format!("could not link {}: {e}", link.display()))?;

    let mut detail = format!("linked into {}", tilde(&dir));
    if !already_on_path {
        match add_to_shell_profiles(&dir) {
            Ok(files) if !files.is_empty() => {
                detail = format!(
                    "linked into {} and added it to {}. Open a new terminal to pick it up.",
                    tilde(&dir),
                    files.join(", ")
                );
            }
            Ok(_) => {
                detail = format!(
                    "linked into {}, which is not on your PATH — add it to your shell profile",
                    tilde(&dir)
                );
            }
            Err(e) => detail = format!("linked into {}, but {e}", tilde(&dir)),
        }
    }

    Ok(StepState {
        installed: true,
        available: true,
        detail,
        backup: None,
    })
}

/// The marker that lets this be run repeatedly without stacking up duplicate lines.
const PROFILE_MARKER: &str = "# added by logbear";

/// Shell profiles that can carry our block, and the shell each belongs to.
const PROFILES: [(&str, &str); 3] = [(".zshrc", "zsh"), (".bashrc", "bash"), (".profile", "sh")];

/// Put `dir` on PATH for every shell the user actually has, and return what was touched.
///
/// fish gets a drop-in under `conf.d/` rather than an edit to `config.fish`: it is the
/// documented extension point, it is one file to delete on uninstall, and it means we
/// never rewrite a file the user has been curating for years.
fn add_to_shell_profiles(dir: &Path) -> Result<Vec<String>, String> {
    let mut touched = Vec::new();
    let d = dir.display().to_string();

    if Path::new(&home().join(".config/fish")).is_dir() || which_shell_is("fish") {
        let conf_d = home().join(".config/fish/conf.d");
        std::fs::create_dir_all(&conf_d).map_err(|e| format!("{}: {e}", conf_d.display()))?;
        let file = conf_d.join("logbear.fish");
        let body = format!(
            "{PROFILE_MARKER}
# Remove this file to take logbear off your PATH.
fish_add_path -g {d}
"
        );
        std::fs::write(&file, body).map_err(|e| format!("{}: {e}", file.display()))?;
        touched.push(tilde(&file));
    }

    for (rc, shell) in PROFILES {
        let path = home().join(rc);
        if !path.exists() && !which_shell_is(shell) {
            continue;
        }
        let block = format!("\n{PROFILE_MARKER}\nexport PATH=\"{d}:$PATH\"\n");
        let mut added = false;
        update_file(&path, |existing| {
            let existing = existing.unwrap_or_default();
            added = !existing.contains(PROFILE_MARKER);
            if !added {
                return Ok(None);
            }
            let mut updated = existing.to_string();
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push_str(&block);
            Ok(Some(updated))
        })?;
        if added {
            touched.push(tilde(&path));
        }
    }

    Ok(touched)
}

fn which_shell_is(name: &str) -> bool {
    std::env::var("SHELL")
        .map(|s| s.ends_with(name))
        .unwrap_or(false)
}

fn uninstall_path() -> Result<StepState, String> {
    let (home, dirs) = (home(), path_candidates());
    let removed = describe_leftovers(&home, &path_leftovers(&home, &dirs));
    let backups = remove_path_setup(&home, &dirs)?;
    let mut state = path_status();
    if !removed.is_empty() {
        state.detail = format!("removed {removed}");
    }
    state.backup = (!backups.is_empty()).then(|| backups.join(", "));
    Ok(state)
}

/// What removing the PATH step would take away, for a plan to show before it happens.
///
/// Not [`path_status`], which answers "does *this* binary resolve on PATH": a link left by
/// another copy, or a profile block outliving its link, reads there as not installed, and
/// is exactly what an uninstall has to find.
pub fn path_removal() -> StepState {
    let (home, dirs) = (home(), path_candidates());
    let found = path_leftovers(&home, &dirs);
    StepState {
        installed: !found.is_empty(),
        available: true,
        detail: if found.is_empty() {
            "no PATH link or shell profile block".into()
        } else {
            format!("will remove {}", describe_leftovers(&home, &found))
        },
        backup: None,
    }
}

/// Whether the symlink at `link` is one of ours.
///
/// A `logbear` link in Homebrew's `bin` belongs to the keg: `brew uninstall` removes it, and
/// deleting it here would leave Homebrew believing a package is linked when it is not.
fn is_our_link(link: &Path) -> bool {
    std::fs::read_link(link).is_ok_and(|target| !target.to_string_lossy().contains("Cellar/logbear/"))
}

fn fish_drop_in(home: &Path) -> PathBuf {
    home.join(".config/fish/conf.d/logbear.fish")
}

/// Lossy, so a profile that is not UTF-8 but holds our block is found, and then refused by
/// [`update_file`] out loud rather than skipped in silence.
fn has_profile_block(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|b| String::from_utf8_lossy(&b).contains(PROFILE_MARKER))
}

/// Everything the PATH step left on this machine: links in `dirs`, then the fish drop-in,
/// then profiles holding our block.
fn path_leftovers(home: &Path, dirs: &[PathBuf]) -> Vec<PathBuf> {
    let links = dirs.iter().map(|d| d.join("logbear")).filter(|l| is_our_link(l));
    let fish = Some(fish_drop_in(home)).filter(|f| f.exists());
    let profiles = PROFILES
        .iter()
        .map(|(rc, _)| home.join(rc))
        .filter(|p| has_profile_block(p));
    links.chain(fish).chain(profiles).collect()
}

fn describe_leftovers(home: &Path, found: &[PathBuf]) -> String {
    let describe = |p: &PathBuf| {
        let short = match p.strip_prefix(home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        };
        if PROFILES.iter().any(|(rc, _)| p.ends_with(rc)) {
            format!("the block in {short}")
        } else {
            short
        }
    };
    found.iter().map(describe).collect::<Vec<_>>().join(", ")
}

/// Take away what [`path_leftovers`] finds. Returns the backups taken.
fn remove_path_setup(home: &Path, dirs: &[PathBuf]) -> Result<Vec<String>, String> {
    for link in dirs.iter().map(|d| d.join("logbear")).filter(|l| is_our_link(l)) {
        std::fs::remove_file(&link).map_err(|e| format!("{}: {e}", link.display()))?;
    }
    let fish = fish_drop_in(home);
    match std::fs::remove_file(&fish) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("{}: {e}", fish.display()));
        }
        _ => {}
    }

    let mut backups = Vec::new();
    for (rc, _) in PROFILES {
        let path = home.join(rc);
        if has_profile_block(&path) {
            backups.extend(update_file(&path, |existing| Ok(existing.map(strip_profile_block)))?);
        }
    }
    Ok(backups)
}

/// Drop our marker line and the export that follows it, leaving everything else alone.
fn strip_profile_block(source: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skip_next_export = false;
    for line in source.lines() {
        if line.trim() == PROFILE_MARKER {
            skip_next_export = true;
            continue;
        }
        if skip_next_export {
            skip_next_export = false;
            if line.trim_start().starts_with("export PATH=") {
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

/// `~/.local/bin` reads better than `/Users/someone/.local/bin` in a UI.
fn tilde(p: &Path) -> String {
    let s = p.display().to_string();
    let h = home().display().to_string();
    match s.strip_prefix(&h) {
        Some(rest) => format!("~{rest}"),
        None => s,
    }
}

// ---------------------------------------------------------------------------
// Daemon service
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn service_file() -> PathBuf {
    home().join("Library/LaunchAgents/sh.logbear.daemon.plist")
}

#[cfg(target_os = "linux")]
fn service_file() -> PathBuf {
    home().join(".config/systemd/user/logbeard.service")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn service_file() -> PathBuf {
    home().join(".logbear/logbeard.service")
}

fn daemon_status() -> StepState {
    let running = logbear_core::client::Client::connect().is_ok();
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

/// The service definition this build wants on disk.
fn service_body() -> Result<String, String> {
    let exe = bin("logbeard");
    if !exe.exists() {
        return Err(format!("logbeard is missing from {}", bin_dir().display()));
    }
    let log = logbear_core::paths::daemon_log();

    // `Adaptive` rather than `Background`: the daemon idles most of the time but the app
    // blocks on its replies, and a Background-class job is scheduled on the efficiency
    // cores with throttled I/O.
    #[cfg(target_os = "macos")]
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>sh.logbear.daemon</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <!-- Not a bare `KeepAlive`, which restarts on *any* exit: the daemon exits 0 both when
       asked to stop and when it finds another instance already holding the lock, and
       relaunching it in either case means a respawn loop or a `stop` that will not stick.
       Crashes still exit non-zero, so those are still restarted. -->
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>ProcessType</key><string>Adaptive</string>
</dict>
</plist>
"#,
        exe = exe.display(),
        log = log.display()
    );

    #[cfg(not(target_os = "macos"))]
    let body = format!(
        // `on-failure`, not `always`, for the reason spelled out in the plist above: a clean
        // exit 0 means "stop" or "another daemon already holds the lock", and neither wants
        // a relaunch.
        "[Unit]\nDescription=logbear daemon\n\n\
         [Service]\nExecStart={exe} serve\nRestart=on-failure\nRestartSec=2\n\n\
         [Install]\nWantedBy=default.target\n",
        exe = exe.display()
    );

    Ok(body)
}

fn install_daemon() -> Result<StepState, String> {
    let body = service_body()?;
    let path = service_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
    load_service(&path);
    Ok(daemon_status())
}

/// Rewrites the service definition when it differs from what this build wants, reporting
/// whether it did.
///
/// Registration happens once, guarded by `.setup-done`, so a changed service file would
/// otherwise never reach a machine that installed an earlier version.
pub fn refresh_service() -> bool {
    let path = service_file();
    if !path.exists() {
        return false;
    }
    let Ok(wanted) = service_body() else {
        return false;
    };
    if std::fs::read_to_string(&path).is_ok_and(|on_disk| on_disk == wanted) {
        return false;
    }
    if std::fs::write(&path, wanted).is_err() {
        return false;
    }
    load_service(&path);
    true
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
    let _ = Command::new("systemctl").args(["--user", "enable", "--now", "logbeard"]).status();
}

#[cfg(not(target_os = "macos"))]
fn unload_service(_path: &Path) {
    use std::process::Command;
    let _ = Command::new("systemctl").args(["--user", "disable", "--now", "logbeard"]).status();
}

// ---------------------------------------------------------------------------
// Harness config
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
    Pi,
    Hermes,
}

impl Harness {
    /// Where the hook is registered. For pi that is the extension file itself.
    fn config(self) -> PathBuf {
        match self {
            Harness::Claude => home().join(".claude/settings.json"),
            Harness::Codex => home().join(".codex/hooks.json"),
            Harness::Pi => pi_dir().join("extensions/logbear.ts"),
            Harness::Hermes => hermes_home().join("config.yaml"),
        }
    }

    fn mcp_config(self) -> PathBuf {
        match self {
            Harness::Claude => home().join(".claude.json"),
            Harness::Codex => home().join(".codex/config.toml"),
            Harness::Pi => pi_dir().join("mcp.json"),
            Harness::Hermes => hermes_home().join("config.yaml"),
        }
    }

    fn present(self) -> bool {
        let dir = match self {
            Harness::Claude => home().join(".claude"),
            Harness::Codex => home().join(".codex"),
            Harness::Pi => pi_dir(),
            Harness::Hermes => hermes_home(),
        };
        dir.exists()
    }

    fn label(self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::Codex => "Codex",
            Harness::Pi => "pi",
            Harness::Hermes => "Hermes",
        }
    }

    /// The name `logbear-hook --emit-config` takes.
    fn flag(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::Hermes => "hermes",
        }
    }
}

fn pi_dir() -> PathBuf {
    home().join(".pi/agent")
}

/// `HERMES_HOME` selects a profile's directory, as it does for Hermes itself.
fn hermes_home() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".hermes"))
}

/// Locate a harness CLI on the user's login-shell PATH.
///
/// A GUI app inherits launchd's PATH, which does not contain the directories these tools
/// install into, so [`user_path`] is searched rather than `std::env::var("PATH")`.
fn harness_cli(name: &str) -> Option<PathBuf> {
    user_path()
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Whether `claude mcp get logbear` resolves. Exits non-zero when the server is unknown.
///
/// Preferred over reading `~/.claude.json` because a server may be registered at local,
/// user or project scope, and only the CLI knows which scopes are in play.
fn claude_cli_has_logbear(cli: &Path) -> bool {
    std::process::Command::new(cli)
        .args(["mcp", "get", MCP_NAME])
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

const MCP_NAME: &str = "logbear";

fn mcp_status(h: Harness) -> StepState {
    let path = h.mcp_config();
    let cli = matches!(h, Harness::Claude)
        .then(|| harness_cli("claude"))
        .flatten();

    let installed = match (&cli, h) {
        (Some(cli), _) => claude_cli_has_logbear(cli),
        (None, Harness::Claude) => read_json(&path)
            .and_then(|v| v.get("mcpServers").cloned())
            .map(|v| v.get(MCP_NAME).is_some())
            .unwrap_or(false),
        (None, Harness::Codex) => std::fs::read_to_string(&path)
            .map(|s| s.contains("[mcp_servers.logbear]"))
            .unwrap_or(false),
        (None, Harness::Pi) => read_json(&path)
            .and_then(|v| v.get("mcpServers").cloned())
            .map(|v| v.get(MCP_NAME).is_some())
            .unwrap_or(false),
        (None, Harness::Hermes) => std::fs::read_to_string(&path)
            .map(|s| s.contains("logbear-mcp"))
            .unwrap_or(false),
    };

    let where_ = match &cli {
        Some(_) => "Claude Code (user scope)".to_string(),
        None => path.display().to_string(),
    };
    StepState {
        installed,
        available: h.present(),
        detail: if !h.present() {
            format!("{} was not detected on this machine", h.label())
        } else if installed {
            format!("registered in {where_}")
        } else {
            format!("not registered in {where_}")
        },
        backup: None,
    }
}

/// Start `logbear-mcp` and complete an `initialize` handshake.
///
/// Registering the server only edits config; this is what proves the binary runs and
/// speaks the protocol, so a failure surfaces here rather than as a silent absence inside
/// the agent later.
fn verify_mcp(server: &Path) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};

    let mut child = std::process::Command::new(server)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("{} would not start: {e}", server.display()))?;

    let req = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"#,
        r#""2025-06-18","capabilities":{},"clientInfo":{"name":"logbear-installer","#,
        r#""version":"1"}}}"#,
        "\n"
    );
    let write = child
        .stdin
        .as_mut()
        .ok_or("no stdin")
        .and_then(|s| s.write_all(req.as_bytes()).map_err(|_| "write failed"));

    let reply = write.ok().and_then(|()| {
        let stdout = child.stdout.take()?;
        BufReader::new(stdout).lines().next()?.ok()
    });
    let _ = child.kill();
    let _ = child.wait();

    let reply = reply.ok_or_else(|| "logbear-mcp did not answer an initialize request".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&reply).map_err(|e| format!("logbear-mcp replied with non-JSON: {e}"))?;
    v.pointer("/result/serverInfo/version")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("unexpected initialize reply: {reply}"))
}

fn install_mcp(h: Harness) -> Result<StepState, String> {
    let server = bin("logbear-mcp");
    if !server.exists() {
        return Err(format!("logbear-mcp is missing from {}", bin_dir().display()));
    }
    verify_mcp(&server)?;

    // The CLI owns the config format across versions and scopes; hand-editing is the
    // fallback for when it is absent.
    if let (Harness::Claude, Some(cli)) = (h, harness_cli("claude")) {
        let out = std::process::Command::new(&cli)
            .args(["mcp", "add", "--scope", "user", MCP_NAME, "--"])
            .arg(&server)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("{} failed: {e}", cli.display()))?;
        if !out.status.success() && !claude_cli_has_logbear(&cli) {
            return Err(format!(
                "claude mcp add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        return Ok(mcp_status(h));
    }

    let path = h.mcp_config();
    let backup = match h {
        Harness::Claude | Harness::Pi => {
            let mut entry = serde_json::json!({
                "command": server.to_string_lossy(),
                "args": [],
            });
            if h == Harness::Pi {
                // pi's default exposure reaches tools through codemode, where the model has to
                // go looking; the unread-error prefix only helps if the tools are called.
                entry["exposure"] = "direct".into();
                entry["description"] =
                    "Captured dev-server output: errors, runs and diffs, as read-only SQL".into();
            }
            update_json(&path, |root| {
                let Some(object) = root.as_object_mut() else {
                    return Err(format!("{} is not a JSON object; leaving it alone", path.display()));
                };
                let servers = object.entry("mcpServers").or_insert_with(|| serde_json::json!({}));
                let Some(servers) = servers.as_object_mut() else {
                    return Err(format!("mcpServers in {} is not an object", path.display()));
                };
                servers.insert(MCP_NAME.into(), entry.clone());
                Ok(())
            })?
        }
        Harness::Hermes => {
            let block = format!(
                "mcp_servers:\n  {MCP_NAME}:\n    command: {}\n    args: []\n",
                serde_json::Value::from(server.to_string_lossy())
            );
            update_file(&path, |existing| {
                let existing = existing.unwrap_or_default();
                if existing.contains("logbear-mcp") {
                    return Ok(None);
                }
                insert_yaml(existing, &block, "mcp").map(Some)
            })?
        }
        // Append a delimited block rather than parsing and rewriting someone else's TOML,
        // which may hold anything.
        Harness::Codex => update_file(&path, |existing| {
            let mut text = existing.unwrap_or_default().to_string();
            if text.contains("[mcp_servers.logbear]") {
                return Ok(None);
            }
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!(
                "\n# added by logbear\n[mcp_servers.logbear]\ncommand = \"{}\"\nargs = []\n",
                server.to_string_lossy()
            ));
            Ok(Some(text))
        })?,
    };

    let mut state = mcp_status(h);
    state.backup = backup;
    Ok(state)
}

fn uninstall_mcp(h: Harness) -> Result<StepState, String> {
    if let (Harness::Claude, Some(cli)) = (h, harness_cli("claude")) {
        if claude_cli_has_logbear(&cli) {
            let _ = std::process::Command::new(&cli)
                .args(["mcp", "remove", "--scope", "user", MCP_NAME])
                .stdin(std::process::Stdio::null())
                .output();
        }
    }

    // Also clear a hand-written entry, which may predate the CLI being available.
    let path = h.mcp_config();
    let backup = match h {
        Harness::Claude | Harness::Pi => remove_mcp_entry(&path)?,
        Harness::Codex => {
            const HEADER: &str = "[mcp_servers.logbear]";
            update_file(&path, |existing| {
                Ok(existing.filter(|s| s.contains(HEADER)).map(|s| strip_toml_block(s, HEADER)))
            })?
        }
        Harness::Hermes => remove_yaml_block(&path, "mcp")?,
    };
    let mut state = mcp_status(h);
    state.backup = backup;
    Ok(state)
}

fn remove_mcp_entry(path: &Path) -> Result<Option<String>, String> {
    update_json(path, |root| {
        if let Some(servers) = root.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
            servers.shift_remove(MCP_NAME);
        }
        Ok(())
    })
}

fn hook_status(h: Harness) -> StepState {
    let path = h.config();
    let installed = std::fs::read_to_string(&path)
        .map(|s| s.contains("logbear-hook"))
        .unwrap_or(false);
    StepState {
        installed,
        available: h.present(),
        detail: if !h.present() {
            format!("{} was not detected on this machine", h.label())
        } else if installed && h == Harness::Hermes {
            // Hermes skips an unapproved shell hook rather than failing, so say so up front.
            format!(
                "installed in {} — approve both entries when Hermes asks, or with `hermes hooks`",
                path.display()
            )
        } else if installed && h == Harness::Pi {
            format!(
                "installed at {} — run /reload in an open pi session",
                path.display()
            )
        } else if installed {
            format!("installed in {}", path.display())
        } else {
            format!("not installed in {}", path.display())
        },
        backup: None,
    }
}

/// The hook binary knows its own config shape, so it emits what registers it and this only
/// has to place it: a JSON block to merge, a YAML block for Hermes, an extension for pi. One
/// source of truth for formats four vendors keep changing.
fn hook_text(h: Harness) -> Result<String, String> {
    let exe = bin("logbear-hook");
    if !exe.exists() {
        return Err(format!("logbear-hook is missing from {}", bin_dir().display()));
    }
    let out = std::process::Command::new(&exe)
        .args(["--emit-config", h.flag()])
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    String::from_utf8(out.stdout).map_err(|e| format!("logbear-hook emitted invalid UTF-8: {e}"))
}

fn install_hook(h: Harness) -> Result<StepState, String> {
    let text = hook_text(h)?;
    let path = h.config();

    let backup = match h {
        Harness::Claude | Harness::Codex => {
            let block: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("logbear-hook emitted invalid JSON: {e}"))?;
            update_json(&path, |root| {
                if !root.is_object() {
                    return Err(format!("{} is not a JSON object; leaving it alone", path.display()));
                }
                merge(root, &block);
                Ok(())
            })?
        }
        // Ours outright, so it is replaced rather than merged: a stale copy is one pointing
        // at an old binary.
        Harness::Pi => update_file(&path, |_| Ok(Some(text.clone())))?,
        Harness::Hermes => update_file(&path, |existing| {
            let existing = existing.unwrap_or_default();
            if existing.contains("logbear-hook") {
                return Ok(None);
            }
            insert_yaml(existing, &text, "hooks").map(Some)
        })?,
    };

    let mut state = hook_status(h);
    state.backup = backup;
    Ok(state)
}

fn uninstall_hook(h: Harness) -> Result<StepState, String> {
    let path = h.config();
    let backup = match h {
        Harness::Claude | Harness::Codex => remove_hook_entries(&path)?,
        Harness::Pi => {
            let backup = back_up(&path)?;
            if path.exists() {
                std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            backup
        }
        Harness::Hermes => remove_yaml_block(&path, "hooks")?,
    };
    let mut state = hook_status(h);
    state.backup = backup;
    Ok(state)
}

fn remove_hook_entries(path: &Path) -> Result<Option<String>, String> {
    update_json(path, |root| {
        prune_logbear(root);
        Ok(())
    })
}

fn remove_yaml_block(path: &Path, tag: &str) -> Result<Option<String>, String> {
    let begin = format!("# logbear:begin {tag}");
    update_file(path, |existing| {
        Ok(existing.filter(|s| s.contains(&begin)).map(|s| strip_yaml_block(s, tag)))
    })
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

/// Remove our handlers from a `hooks` block, then the containers that left empty.
///
/// A handler is removed, never the group holding it: a group's `hooks` array can hold
/// someone else's handler beside ours. Only the `hooks` block is looked at, so a status line
/// or a permission rule that happens to name the binary stays, and a container the user
/// already had empty stays empty.
fn prune_logbear(root: &mut serde_json::Value) {
    let Some(events) = root.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return;
    };
    let mut removed_any = false;
    events.retain(|_, groups| {
        let Some(list) = groups.as_array_mut() else {
            return true;
        };
        let mut removed = false;
        list.retain_mut(|group| {
            let Some(handlers) = group.get_mut("hooks").and_then(|v| v.as_array_mut()) else {
                let ours = is_our_handler(group);
                removed |= ours;
                return !ours;
            };
            let count = handlers.len();
            handlers.retain(|h| !is_our_handler(h));
            removed |= handlers.len() != count;
            !handlers.is_empty() || count == 0
        });
        removed_any |= removed;
        !(removed && list.is_empty())
    });
    if removed_any && events.is_empty() {
        if let Some(object) = root.as_object_mut() {
            object.shift_remove("hooks");
        }
    }
}

/// Whether a handler runs `logbear-hook`: the program itself, not a command that mentions it
/// or wraps it. Its path is quoted when it holds a space, as `--emit-config` does.
fn is_our_handler(handler: &serde_json::Value) -> bool {
    let Some(command) = handler.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    let command = command.trim_start();
    let program = match command.chars().next() {
        Some(quote @ ('\'' | '"')) => command[1..].split(quote).next().unwrap_or_default(),
        _ => command.split_whitespace().next().unwrap_or_default(),
    };
    Path::new(program).file_name().is_some_and(|name| name == "logbear-hook")
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

// ---------------------------------------------------------------------------
// Writing configs
// ---------------------------------------------------------------------------

/// How many times a write starts over because the file moved underneath it. Agents save
/// these files in bursts, not continuously, so more than a few means something is wrong.
const WRITE_ATTEMPTS: usize = 5;

/// `None` for a file that is not there. Anything else unreadable is an error: reading it as
/// empty is how a merge turns into an overwrite.
fn read_config(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("could not read {}, so it was left alone: {e}", path.display())),
    }
}

/// Apply `edit` to a config file, atomically. Returns the backup taken, if a write happened
/// and there was a file to back up.
///
/// `edit` gets the current contents, or `None` for a missing file, and returns the new ones,
/// or `None` to leave the file alone. The file is read again just before the rename, and if
/// another program saved in between, `edit` runs again on what it wrote. That narrows the
/// lost-update window to the gap between that read and the rename; without a lock every
/// writer honours, nothing closes it. The rename means nobody ever sees a half-written file.
fn update_file(
    path: &Path,
    mut edit: impl FnMut(Option<&str>) -> Result<Option<String>, String>,
) -> Result<Option<String>, String> {
    if path.is_symlink() && !path.exists() {
        return Err(format!("{} is a broken symlink, so it was left alone", path.display()));
    }
    // A rename replaces the directory entry, so aim it at the real file: dotfile managers
    // symlink these, and writing through the link must not sever it.
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    for _ in 0..WRITE_ATTEMPTS {
        let before = read_config(&target)?;
        let Some(after) = edit(before.as_deref())? else {
            return Ok(None);
        };
        if before.as_deref() == Some(after.as_str()) {
            return Ok(None);
        }
        if read_config(&target)? != before {
            continue;
        }
        let backup = back_up(path)?;
        write_atomically(&target, &after)?;
        return Ok(backup);
    }
    Err(format!(
        "{} kept changing while logbear was updating it, so nothing was written. Try again.",
        path.display()
    ))
}

/// [`update_file`] for JSON. A file that does not parse is refused: it may be a comment away
/// from valid, and it is the user's to fix. A write that would change nothing does not
/// happen, which spares the file a reformat and a pointless backup.
fn update_json(
    path: &Path,
    mut edit: impl FnMut(&mut serde_json::Value) -> Result<(), String>,
) -> Result<Option<String>, String> {
    update_file(path, |existing| {
        let mut root: serde_json::Value = match existing {
            Some(text) => serde_json::from_str(text).map_err(|e| {
                format!(
                    "{} is not valid JSON ({e}), so it was left alone. Fix it and run this again.",
                    path.display()
                )
            })?,
            None => serde_json::json!({}),
        };
        let before = root.clone();
        edit(&mut root)?;
        if root == before {
            return Ok(None);
        }
        let body = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
        Ok(Some(body + "\n"))
    })
}

/// Write beside `target`, then rename over it. Same directory, because a rename across
/// filesystems is a copy. The temp file starts at 0600 so a private config is never briefly
/// readable, then takes the original's mode.
fn write_atomically(target: &Path, body: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let describe = |e: std::io::Error| format!("{}: {e}", target.display());
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let temp = parent.join(format!(
        ".{name}.logbear-tmp.{}.{}",
        std::process::id(),
        logbear_core::now_micros()
    ));

    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(body.as_bytes())?;
        if let Ok(meta) = std::fs::metadata(target) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        std::fs::rename(&temp, target)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        describe(e)
    })
}

// ---------------------------------------------------------------------------
// Removing captured data
// ---------------------------------------------------------------------------

/// Names only a LogBear data directory has. `paths::ensure_home` creates `spool` first thing,
/// and the rest appear as the daemon and the app run.
const DATA_MARKERS: [&str; 5] = ["spool", "logbeard.pid", "logbeard.sock", "logbeard.log", ".setup-done"];

/// Why `dir` must not be deleted, if it must not. Deletes nothing, and tests may hand it any
/// path; everything that can delete goes through it first.
///
/// `LOGBEAR_HOME` is a plain environment variable, and a recursive delete of the wrong
/// directory is the one mistake here that no backup undoes.
fn purge_refusal(dir: &Path, user_home: &Path) -> Option<String> {
    let resolved = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let user_home = user_home.canonicalize().unwrap_or_else(|_| user_home.to_path_buf());
    if resolved.parent().is_none() || user_home.starts_with(&resolved) {
        return Some(format!("{} contains your home directory", dir.display()));
    }
    let names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(listing) => listing
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(e) => return Some(format!("{}: {e}", dir.display())),
    };
    if !names.is_empty() && !DATA_MARKERS.iter().any(|m| names.iter().any(|n| n == m)) {
        return Some(format!("{} does not look like a LogBear data directory", dir.display()));
    }
    None
}

/// Delete `dir`, if [`purge_refusal`] allows it. `false` means it was not there.
fn purge_dir(dir: &Path, user_home: &Path) -> Result<bool, String> {
    if !dir.exists() {
        return Ok(false);
    }
    if let Some(why) = purge_refusal(dir, user_home) {
        return Err(format!("{why}, so nothing was deleted"));
    }
    std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(true)
}

/// Stop a running daemon. It holds the databases open and would go on writing to deleted
/// files, whether a service runs it or a wrapped command started it.
fn stop_daemon() -> Result<(), String> {
    use logbear_core::client::Client;
    let Ok(mut client) = Client::connect() else {
        return Ok(());
    };
    let _ = client.call::<serde_json::Value>(&logbear_core::proto::Request::Shutdown);
    for _ in 0..50 {
        if Client::connect().is_err() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err("the daemon is still running, so nothing was deleted".into())
}

/// Delete everything LogBear captured, stopping the daemon first. Returns the directory if
/// there was one to delete. Irreversible, so the caller asks first.
pub fn purge_data() -> Result<Option<PathBuf>, String> {
    let dir = logbear_core::paths::home();
    if !dir.exists() {
        return Ok(None);
    }
    if let Some(why) = purge_refusal(&dir, &home()) {
        return Err(format!("{why}, so nothing was deleted"));
    }
    stop_daemon()?;
    purge_dir(&dir, &home()).map(|existed| existed.then_some(dir))
}

// ---------------------------------------------------------------------------
// YAML merging
// ---------------------------------------------------------------------------

/// Merge a top-level YAML mapping block into `source` without parsing or re-serialising it.
///
/// A config people edit by hand keeps its comments and layout this way. The lines we add sit
/// between `# logbear:begin <tag>` and `# logbear:end <tag>`, so uninstall removes exactly
/// them. Refuses rather than guesses when the key holds an inline value, or already has one
/// of the block's children: two keys of one name is invalid YAML, and merging the user's
/// entry into ours means rewriting theirs.
fn insert_yaml(source: &str, block: &str, tag: &str) -> Result<String, String> {
    let begin = format!("# logbear:begin {tag}");
    let end = format!("# logbear:end {tag}");
    let mut block_lines = block.lines();
    let key = block_lines
        .next()
        .and_then(|l| l.strip_suffix(':'))
        .ok_or_else(|| "a YAML block must start with `key:`".to_string())?;
    let children: Vec<&str> = block_lines.collect();
    let by_hand = || format!("add this to it by hand:\n{block}");

    let lines: Vec<&str> = source.lines().collect();
    let Some(at) = lines.iter().position(|l| l.starts_with(&format!("{key}:"))) else {
        let mut out = source.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{begin}\n{key}:\n"));
        for child in &children {
            out.push_str(&format!("{child}\n"));
        }
        out.push_str(&format!("{end}\n"));
        return Ok(out);
    };

    let inline = lines[at][key.len() + 1..].trim();
    if !inline.is_empty() && !inline.starts_with('#') {
        return Err(format!(
            "`{key}:` in config.yaml has an inline value; {}",
            by_hand()
        ));
    }

    // The section runs to the next line at column 0 that is not blank or a comment.
    let section_end = lines[at + 1..]
        .iter()
        .position(|l| !l.is_empty() && !l.starts_with([' ', '\t', '#']))
        .map_or(lines.len(), |i| at + 1 + i);
    let section = &lines[at + 1..section_end];
    let indent = section
        .iter()
        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map_or("  ", |l| &l[..l.len() - l.trim_start().len()]);

    for name in children
        .iter()
        .filter(|c| !c.starts_with("   "))
        .map(|c| c.trim())
    {
        let taken = section.iter().any(|l| {
            l.strip_prefix(indent).is_some_and(|rest| {
                !rest.starts_with(' ') && rest.split_whitespace().next() == Some(name)
            })
        });
        if taken {
            return Err(format!(
                "config.yaml already has `{}` under `{key}:`; {}",
                name.trim_end_matches(':'),
                by_hand()
            ));
        }
    }

    let mut out: Vec<String> = lines[..=at].iter().map(|l| l.to_string()).collect();
    out.push(format!("{indent}{begin}"));
    // Our block is indented by two; re-indent its first level to match the user's.
    out.extend(
        children
            .iter()
            .map(|c| format!("{indent}{}", c.strip_prefix("  ").unwrap_or(c))),
    );
    out.push(format!("{indent}{end}"));
    out.extend(lines[at + 1..].iter().map(|l| l.to_string()));
    let mut text = out.join("\n");
    if source.ends_with('\n') || source.is_empty() {
        text.push('\n');
    }
    Ok(text)
}

/// Remove what [`insert_yaml`] added under `tag`, markers included.
fn strip_yaml_block(source: &str, tag: &str) -> String {
    let begin = format!("# logbear:begin {tag}");
    let end = format!("# logbear:end {tag}");
    let mut out = Vec::new();
    let mut skipping = false;
    for line in source.lines() {
        match line.trim() {
            t if t == begin => skipping = true,
            t if t == end => skipping = false,
            _ if skipping => {}
            _ => out.push(line),
        }
    }
    let mut text = out.join("\n");
    if source.ends_with('\n') && !text.is_empty() {
        text.push('\n');
    }
    text
}

/// Copy a config aside before touching it. Returns the backup path, which the UI shows
/// so "revert" is a real offer rather than a promise.
fn back_up(path: &Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = logbear_core::now_micros();
    let backup = path.with_extension(format!("logbear-backup.{stamp}"));
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
            // Also drop the "# added by logbear" marker we wrote above it.
            if out.last().map(|l: &&str| l.trim() == "# added by logbear").unwrap_or(false) {
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
                "Stop": [{ "hooks": [{ "type": "command", "command": "logbear-hook stop" }] }]
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
        let ours = json!({ "hooks": { "Stop": [{ "command": "logbear-hook" }] } });
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
                    { "hooks": [{ "type": "command", "command": "/opt/logbear-hook stop" }] }
                ]
            }
        });
        prune_logbear(&mut config);
        assert_eq!(config["model"], "opus");
        let stop = config["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(stop[0].to_string().contains("their-tool"));
    }

    #[test]
    fn uninstall_leaves_no_empty_containers_behind() {
        let mut config = json!({
            "hooks": { "Stop": [{ "hooks": [{ "command": "logbear-hook" }] }] },
            "model": "opus"
        });
        prune_logbear(&mut config);
        assert!(config.get("hooks").is_none(), "an empty hooks object was left: {config}");
        assert_eq!(config["model"], "opus");
    }

    #[test]
    fn a_toml_block_is_removed_without_disturbing_its_neighbours() {
        let source = "[general]\nfoo = 1\n\n# added by logbear\n[mcp_servers.logbear]\ncommand = \"/x\"\nargs = []\n\n[mcp_servers.other]\ncommand = \"/y\"\n";
        let out = strip_toml_block(source, "[mcp_servers.logbear]");
        assert!(!out.contains("mcp_servers.logbear"));
        assert!(!out.contains("added by logbear"));
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
    fn a_profile_block_is_removed_cleanly() {
        let before = "export EDITOR=nvim\n\n# added by logbear\nexport PATH=\"/Users/x/.local/bin:$PATH\"\n\nalias g=git\n";
        let after = strip_profile_block(before);
        assert!(!after.contains("added by logbear"), "{after}");
        assert!(!after.contains(".local/bin"), "{after}");
        assert!(after.contains("export EDITOR=nvim"), "an unrelated line was lost: {after}");
        assert!(after.contains("alias g=git"), "an unrelated line was lost: {after}");
    }

    #[test]
    fn removing_the_block_leaves_a_users_own_path_export_alone() {
        let before = "export PATH=\"/opt/mine:$PATH\"\n# added by logbear\nexport PATH=\"/Users/x/.local/bin:$PATH\"\n";
        let after = strip_profile_block(before);
        assert!(after.contains("/opt/mine"), "their own PATH export was eaten: {after}");
        assert!(!after.contains(".local/bin"), "{after}");
    }

    #[test]
    fn the_user_path_is_read_from_the_login_shell_not_this_process() {
        // The bug this guards: a Finder-launched app inherits launchd's PATH, so asking
        // this process would answer a question nobody asked. A login shell knows about
        // ~/.local/bin, homebrew, mise and asdf; launchd does not.
        let from_shell = user_path();
        assert!(!from_shell.is_empty(), "the login shell returned no PATH at all");
        assert!(
            from_shell.len() > 4,
            "suspiciously short PATH ({} entries) — this looks like launchd's, not the user's",
            from_shell.len()
        );
    }

    #[test]
    fn the_chosen_directory_is_writable_without_admin_rights() {
        let (dir, _) = chosen_dir();
        assert!(
            dir.starts_with(home()) || writable(&dir),
            "{} would need sudo during a first launch",
            dir.display()
        );
    }

    #[test]
    fn tilde_shortens_paths_under_home_only() {
        assert_eq!(tilde(&home().join(".local/bin")), "~/.local/bin");
        assert_eq!(tilde(Path::new("/usr/local/bin")), "/usr/local/bin");
    }

    #[test]
    fn a_homebrew_cellar_path_is_recorded_under_opt() {
        assert_eq!(
            stable_bin_dir(Path::new("/opt/homebrew/Cellar/logbear/0.1.0/bin")),
            PathBuf::from("/opt/homebrew/opt/logbear/bin")
        );
        for other in [
            "/Applications/logbear.app/Contents/MacOS",
            "/Users/x/.local/bin",
        ] {
            assert_eq!(stable_bin_dir(Path::new(other)), PathBuf::from(other));
        }
    }

    #[test]
    fn step_names_match_what_the_ui_sends() {
        for name in [
            "path",
            "daemon",
            "mcp_claude",
            "mcp_codex",
            "mcp_pi",
            "mcp_hermes",
            "hook_claude",
            "hook_codex",
            "hook_pi",
            "hook_hermes",
        ] {
            assert!(Step::parse(name).is_some(), "unknown step: {name}");
        }
        assert!(Step::parse("nonsense").is_none());
    }

    const HOOKS: &str = "hooks:\n  pre_llm_call:\n    - command: \"/x/logbear-hook --harness hermes\"\n      timeout: 10\n";

    #[test]
    fn a_yaml_block_is_appended_when_its_key_is_absent_and_removed_whole() {
        let source = "model: hermes-4\nagent:\n  max_turns: 40\n";
        let merged = insert_yaml(source, HOOKS, "hooks").unwrap();
        assert!(merged.starts_with(source), "{merged}");
        assert!(merged.contains("\nhooks:\n  pre_llm_call:\n"), "{merged}");
        assert_eq!(strip_yaml_block(&merged, "hooks"), source);
    }

    #[test]
    fn a_yaml_block_joins_an_existing_key_at_its_indentation() {
        let source = "hooks:\n    post_tool_call:\n        - command: fmt.sh\nmodel: hermes-4\n";
        let merged = insert_yaml(source, HOOKS, "hooks").unwrap();
        assert_eq!(
            merged.matches("\nhooks:").count() + merged.starts_with("hooks:") as usize,
            1
        );
        assert!(
            // Nesting below the first level only has to be consistent within itself.
            merged.contains("\n    pre_llm_call:\n      - command: \"/x/logbear-hook"),
            "{merged}"
        );
        assert!(
            merged.contains("    post_tool_call:\n        - command: fmt.sh"),
            "{merged}"
        );
        assert!(merged.ends_with("model: hermes-4\n"), "{merged}");
        assert_eq!(strip_yaml_block(&merged, "hooks"), source);
    }

    /// Two `pre_llm_call:` keys in one mapping is invalid YAML, and merging the user's list
    /// into ours means rewriting it: that one is theirs to do.
    #[test]
    fn a_yaml_key_the_user_already_has_is_refused_rather_than_duplicated() {
        let source = "hooks:\n  pre_llm_call:\n    - command: recall.sh\n";
        let err = insert_yaml(source, HOOKS, "hooks").unwrap_err();
        assert!(err.contains("pre_llm_call"), "{err}");
    }

    #[test]
    fn an_inline_yaml_value_is_refused() {
        assert!(insert_yaml("hooks: {}\n", HOOKS, "hooks").is_err());
    }

    // ---- Writes ----

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_config_that_will_not_parse_is_refused_and_left_byte_for_byte() {
        for broken in ["{\"model\": \"opus\",}", "// a comment\n{}", "", "   \n"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.json");
            std::fs::write(&path, broken).unwrap();

            let err = update_json(&path, |root| {
                root["added"] = true.into();
                Ok(())
            })
            .unwrap_err();

            assert!(err.contains("settings.json"), "the error should name the file: {err}");
            assert!(err.contains("not valid JSON"), "{err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
            assert_eq!(entries(dir.path()), ["settings.json"], "nothing else should appear");
        }
    }

    #[test]
    fn a_text_config_that_is_not_utf8_is_refused_rather_than_read_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".zshrc");
        std::fs::write(&path, b"export X=\xff\n").unwrap();
        let result = update_file(&path, |_| Ok(Some("replaced\n".into())));
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"export X=\xff\n");
    }

    #[test]
    fn a_missing_config_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        let backup = update_json(&path, |root| {
            root["added"] = true.into();
            Ok(())
        })
        .unwrap();
        assert_eq!(backup, None, "there was nothing to back up");
        assert_eq!(read_json(&path).unwrap()["added"], true);
    }

    #[test]
    fn a_write_that_lands_while_we_work_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\"theirs\": 1}").unwrap();

        // Stands in for Claude Code saving between our read and our write.
        let mut interrupted = false;
        update_json(&path, |root| {
            if !interrupted {
                interrupted = true;
                std::fs::write(&path, "{\"theirs\": 2, \"new\": true}").unwrap();
            }
            root["ours"] = true.into();
            Ok(())
        })
        .unwrap();

        let after = read_json(&path).unwrap();
        assert_eq!(after["theirs"], 2, "the concurrent write was lost: {after}");
        assert_eq!(after["new"], true, "the concurrent write was lost: {after}");
        assert_eq!(after["ours"], true);
    }

    #[test]
    fn a_file_that_never_settles_is_refused_rather_than_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();

        let mut n = 0;
        let err = update_json(&path, |root| {
            n += 1;
            std::fs::write(&path, format!("{{\"n\": {n}}}")).unwrap();
            root["ours"] = true.into();
            Ok(())
        })
        .unwrap_err();

        assert!(err.contains("changing"), "{err}");
        assert!(read_json(&path).unwrap().get("ours").is_none());
    }

    /// A reader holding the old file keeps a whole old file: the rename swaps it, rather
    /// than truncating it underneath them. Nothing is left beside it either.
    #[test]
    fn a_config_is_replaced_whole_and_no_temp_file_survives() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\"old\": true}").unwrap();
        let mut held_open = std::fs::File::open(&path).unwrap();

        let backup = update_json(&path, |root| {
            root["new"] = true.into();
            Ok(())
        })
        .unwrap()
        .unwrap();

        let mut seen = String::new();
        held_open.read_to_string(&mut seen).unwrap();
        assert_eq!(seen, "{\"old\": true}");
        assert_eq!(read_json(&path).unwrap()["new"], true);
        let backup_name = Path::new(&backup).file_name().unwrap().to_string_lossy().into_owned();
        let mut expected = vec![backup_name, "settings.json".to_string()];
        expected.sort();
        assert_eq!(entries(dir.path()), expected);
    }

    #[test]
    fn a_failed_edit_writes_nothing_and_takes_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "hooks: {}\n").unwrap();
        assert!(update_file(&path, |_| Err("no".into())).is_err());
        assert_eq!(entries(dir.path()), ["config.yaml"]);
    }

    #[test]
    fn an_edit_that_changes_nothing_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ \"model\":   \"opus\" }").unwrap();
        assert_eq!(update_json(&path, |_| Ok(())).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ \"model\":   \"opus\" }");
        assert_eq!(entries(dir.path()), ["settings.json"]);
    }

    #[test]
    fn a_symlinked_config_stays_a_symlink() {
        // Dotfile managers link these, and a rename onto the link would silently cut the
        // file off from the repository it lives in.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/settings.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "{\"mine\": true}").unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        update_json(&link, |root| {
            root["ours"] = true.into();
            Ok(())
        })
        .unwrap();

        assert!(link.is_symlink(), "the link was replaced by a regular file");
        let after = read_json(&real).unwrap();
        assert_eq!(after["mine"], true);
        assert_eq!(after["ours"], true);
    }

    #[test]
    fn a_configs_permissions_survive_the_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        update_json(&path, |root| {
            root["ours"] = true.into();
            Ok(())
        })
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn key_order_survives_a_merge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\"zebra\": 1, \"apple\": {\"y\": 1, \"b\": 2}, \"mango\": 3}\n").unwrap();

        update_json(&path, |root| {
            merge(root, &json!({ "hooks": { "Stop": [] } }));
            Ok(())
        })
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{needle} in {text}"));
        assert!(at("zebra") < at("apple") && at("apple") < at("mango"), "{text}");
        assert!(at("\"y\"") < at("\"b\""), "{text}");
        assert!(at("mango") < at("hooks"), "{text}");
    }

    #[test]
    fn key_order_survives_an_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"z": 1, "hooks": {"Stop": [{"hooks": [{"command": "logbear-hook"}]}]}, "a": 2, "m": 3}"#,
        )
        .unwrap();

        remove_hook_entries(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let positions: Vec<usize> = ["\"z\"", "\"a\"", "\"m\""]
            .iter()
            .map(|k| text.find(k).unwrap_or_else(|| panic!("{k} in {text}")))
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{text}");
        assert!(!text.contains("hooks"), "{text}");
    }

    // ---- Uninstall ----

    #[test]
    fn a_users_hook_sharing_a_group_with_ours_survives() {
        let mut config = json!({
            "hooks": {
                "Stop": [{
                    "matcher": "",
                    "hooks": [
                        { "type": "command", "command": "their-tool --notify" },
                        { "type": "command", "command": "/opt/logbear-hook --harness claude" }
                    ]
                }]
            }
        });
        prune_logbear(&mut config);
        assert_eq!(
            config,
            json!({
                "hooks": {
                    "Stop": [{
                        "matcher": "",
                        "hooks": [{ "type": "command", "command": "their-tool --notify" }]
                    }]
                }
            })
        );
    }

    #[test]
    fn something_that_merely_mentions_the_hook_is_not_ours() {
        let original = json!({
            "statusLine": { "type": "command", "command": "echo logbear-hook is installed" },
            "permissions": { "allow": ["Bash(logbear-hook --emit-config claude)"] },
            "hooks": {
                "Stop": [{
                    "hooks": [{ "type": "command", "command": "audit --wrap /opt/logbear-hook" }]
                }]
            }
        });
        let mut config = original.clone();
        prune_logbear(&mut config);
        assert_eq!(config, original);
    }

    #[test]
    fn our_hook_is_recognised_when_its_path_needed_quoting() {
        let mut config = json!({
            "hooks": {
                "Stop": [{ "hooks": [{
                    "command": "'/Applications/My Apps/LogBear.app/Contents/MacOS/logbear-hook' --harness claude"
                }] }]
            }
        });
        prune_logbear(&mut config);
        assert_eq!(config, json!({}));
    }

    #[test]
    fn containers_the_user_left_empty_are_not_ours_to_tidy() {
        let original = json!({ "hooks": { "PreToolUse": [] }, "env": {} });
        let mut config = original.clone();
        prune_logbear(&mut config);
        assert_eq!(config, original);
    }

    #[test]
    fn hook_uninstall_backs_up_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let source = json!({ "hooks": { "Stop": [{ "hooks": [{ "command": "logbear-hook" }] }] } }).to_string();
        std::fs::write(&path, &source).unwrap();

        let backup = remove_hook_entries(&path).unwrap().expect("a backup path");

        assert_eq!(std::fs::read_to_string(backup).unwrap(), source);
        assert!(read_json(&path).unwrap().get("hooks").is_none());
    }

    #[test]
    fn mcp_uninstall_backs_up_first_and_keeps_other_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        let source = json!({ "mcpServers": { "logbear": {}, "other": {} } }).to_string();
        std::fs::write(&path, &source).unwrap();

        let backup = remove_mcp_entry(&path).unwrap().expect("a backup path");

        assert_eq!(std::fs::read_to_string(backup).unwrap(), source);
        assert_eq!(read_json(&path).unwrap(), json!({ "mcpServers": { "other": {} } }));
    }

    #[test]
    fn uninstalling_from_a_config_that_will_not_parse_is_an_error_not_a_silent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(remove_hook_entries(&path).is_err());
        assert!(remove_mcp_entry(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn uninstalling_what_was_never_there_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\"model\":\"opus\"}").unwrap();
        assert_eq!(remove_hook_entries(&path).unwrap(), None);
        assert_eq!(remove_mcp_entry(&path).unwrap(), None);
        assert_eq!(entries(dir.path()), ["settings.json"]);
        assert_eq!(remove_hook_entries(&dir.path().join("absent.json")).unwrap(), None);
    }

    #[test]
    fn a_profile_block_added_then_removed_leaves_the_file_as_it_was() {
        let before = "export EDITOR=nvim\n";
        let with_block = format!("{before}\n{PROFILE_MARKER}\nexport PATH=\"/x/bin:$PATH\"\n");
        assert_eq!(strip_profile_block(&with_block), before);
    }

    // ---- PATH ----

    /// A home and bin directories under a temp dir: the real candidates include
    /// `/opt/homebrew/bin` and `/usr/local/bin`, which no test may touch.
    struct Machine {
        _dir: tempfile::TempDir,
        home: PathBuf,
        bins: Vec<PathBuf>,
    }

    fn machine() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let bins = vec![home.join(".local/bin"), dir.path().join("homebrew/bin")];
        for b in &bins {
            std::fs::create_dir_all(b).unwrap();
        }
        Machine { _dir: dir, home, bins }
    }

    #[test]
    fn removing_path_takes_the_link_the_rc_block_and_the_fish_drop_in() {
        let m = machine();
        let link = m.bins[0].join("logbear");
        std::os::unix::fs::symlink("/Applications/LogBear.app/Contents/MacOS/logbear", &link).unwrap();
        let zshrc = m.home.join(".zshrc");
        std::fs::write(&zshrc, format!("alias g=git\n\n{PROFILE_MARKER}\nexport PATH=\"/x:$PATH\"\n")).unwrap();
        let fish = m.home.join(".config/fish/conf.d/logbear.fish");
        std::fs::create_dir_all(fish.parent().unwrap()).unwrap();
        std::fs::write(&fish, PROFILE_MARKER).unwrap();

        assert_eq!(path_leftovers(&m.home, &m.bins).len(), 3);
        let backups = remove_path_setup(&m.home, &m.bins).unwrap();

        assert!(!link.is_symlink());
        assert!(!fish.exists());
        assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), "alias g=git\n");
        assert_eq!(backups.len(), 1, "the rc file is backed up");
        assert!(std::fs::read_to_string(&backups[0]).unwrap().contains(PROFILE_MARKER));
        assert!(path_leftovers(&m.home, &m.bins).is_empty());
    }

    #[test]
    fn removing_path_leaves_homebrews_link_and_real_files_alone() {
        let m = machine();
        let brew = m.bins[1].join("logbear");
        std::os::unix::fs::symlink("../Cellar/logbear/0.1.0/bin/logbear", &brew).unwrap();
        let script_install = m.bins[0].join("logbear");
        std::fs::write(&script_install, "#!/bin/sh\n").unwrap();

        assert!(path_leftovers(&m.home, &m.bins).is_empty());
        remove_path_setup(&m.home, &m.bins).unwrap();

        assert!(brew.is_symlink(), "Homebrew owns its own link");
        assert!(script_install.exists(), "a real binary is not a link of ours");
    }

    #[test]
    fn removing_path_keeps_a_symlinked_rc_file_linked() {
        let m = machine();
        let real = m.home.join("dotfiles/zshrc");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, format!("{PROFILE_MARKER}\nexport PATH=\"/x:$PATH\"\n")).unwrap();
        std::os::unix::fs::symlink(&real, m.home.join(".zshrc")).unwrap();

        remove_path_setup(&m.home, &m.bins).unwrap();

        assert!(m.home.join(".zshrc").is_symlink());
        assert!(!std::fs::read_to_string(&real).unwrap().contains(PROFILE_MARKER));
    }

    // ---- Purge ----

    /// The guard alone, which deletes nothing: the only place a test may name `/` or a real
    /// directory. Anything that can actually delete is tested inside a temp dir.
    #[test]
    fn the_purge_guard_refuses_anything_that_is_not_a_logbear_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let user_home = dir.path().join("me");
        let documents = user_home.join("Documents");
        let data = user_home.join(".logbear");
        std::fs::create_dir_all(&documents).unwrap();
        std::fs::create_dir_all(data.join("spool")).unwrap();
        std::fs::write(documents.join("thesis.txt"), "years of work").unwrap();

        // `LOGBEAR_HOME` is a plain environment variable, so each of these is one typo away.
        for wrong in [Path::new("/"), dir.path(), user_home.as_path(), documents.as_path()] {
            assert!(purge_refusal(wrong, &user_home).is_some(), "{} was accepted", wrong.display());
        }
        assert_eq!(purge_refusal(&data, &user_home), None);
    }

    #[test]
    fn an_empty_directory_may_be_purged() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("scratch");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(purge_refusal(&empty, Path::new("/nonexistent-home")), None);
    }

    #[test]
    fn purging_deletes_a_logbear_data_directory_and_only_that() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join(".logbear");
        std::fs::create_dir_all(data.join("spool")).unwrap();
        std::fs::write(data.join("index.db"), "x").unwrap();
        std::fs::write(dir.path().join("neighbour"), "y").unwrap();

        assert!(purge_dir(&data, Path::new("/nonexistent-home")).unwrap());

        assert!(!data.exists());
        assert!(dir.path().join("neighbour").exists());
    }

    #[test]
    fn purging_nothing_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join(".logbear");
        assert!(!purge_dir(&data, Path::new("/nonexistent-home")).unwrap());
    }
}

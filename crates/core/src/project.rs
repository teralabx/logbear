//! Project resolution and run stamping.
//!
//! Git state is read by shelling out to `git` rather than linking libgit2, which avoids
//! reimplementing revision walking for the diff view. Every call is best effort: a missing
//! or broken repository degrades to `None` rather than an error.

use crate::model::RunStamp;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Lockfiles hashed into every run stamp.
pub const LOCKFILES: &[&str] = &[
    "mix.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
    "bun.lock",
    "Gemfile.lock",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "requirements.txt",
    "go.sum",
    "composer.lock",
    "pubspec.lock",
    "Package.resolved",
    "gradle.lockfile",
];

/// The git root containing `dir`, or `dir` itself when it is not in a repository.
pub fn project_root(dir: &Path) -> PathBuf {
    git(dir, &["rev-parse", "--show-toplevel"])
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| dir.to_path_buf())
}

/// Human-facing project name, taken from the root directory's name.
pub fn project_name(root: &Path) -> String {
    root.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

/// Whether `run_cwd` belongs to the project rooted at `root`.
///
/// Containment rather than equality, so a server started in a monorepo subdirectory such
/// as `apps/api` still resolves to the repo root.
pub fn contains(root: &Path, run_cwd: &Path) -> bool {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let cwd = run_cwd.canonicalize().unwrap_or_else(|_| run_cwd.to_path_buf());
    cwd.starts_with(&root)
}

/// Builds the stamp recorded for a run.
///
/// Runs beside the wrapped command rather than ahead of it. The git calls run concurrently,
/// so a short command pays for the slowest one rather than the sum of them.
pub fn stamp(command: Vec<String>, cwd: &Path, cols: u16, rows: u16) -> RunStamp {
    let (root, git_sha, git_branch, git_dirty_hash) = std::thread::scope(|s| {
        let root = s.spawn(|| project_root(cwd));
        let sha = s.spawn(|| git(cwd, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string()));
        let branch = s.spawn(|| {
            git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
                .map(|s| s.trim().to_string())
                .filter(|s| s != "HEAD")
        });
        let dirty = s.spawn(|| dirty_hash(cwd));
        (
            root.join().unwrap_or_else(|_| cwd.to_path_buf()),
            sha.join().unwrap_or(None),
            branch.join().unwrap_or(None),
            dirty.join().unwrap_or(None),
        )
    });
    let mut stamp = quick_stamp(command, cwd, cols, rows);
    stamp.project_name = project_name(&root);
    stamp.project_root = root.to_string_lossy().into_owned();
    stamp.git_sha = git_sha;
    stamp.git_branch = git_branch;
    stamp.git_dirty_hash = git_dirty_hash;
    stamp.lockfile_hashes = lockfile_hashes(&root);
    stamp
}

/// A stamp that runs nothing: enough to file the run under the right project when the real
/// one is not ready, as when a command finishes before git has answered.
pub fn quick_stamp(command: Vec<String>, cwd: &Path, cols: u16, rows: u16) -> RunStamp {
    let root = cwd
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd);
    RunStamp {
        command,
        cwd: cwd.to_string_lossy().into_owned(),
        project_name: project_name(root),
        project_root: root.to_string_lossy().into_owned(),
        env_keys: env_keys(),
        term_cols: cols,
        term_rows: rows,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        shell: std::env::var("SHELL")
            .ok()
            .and_then(|s| PathBuf::from(s).file_name().map(|f| f.to_string_lossy().into_owned())),
        ..Default::default()
    }
}

/// Hash of the uncommitted changes, distinguishing two runs at the same SHA with
/// different local edits. `None` when nothing is uncommitted.
pub fn dirty_hash(cwd: &Path) -> Option<String> {
    let diff = git(cwd, &["diff", "HEAD"])?;
    let untracked = git(cwd, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
    if diff.trim().is_empty() && untracked.trim().is_empty() {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(diff.as_bytes());
    hasher.update(untracked.as_bytes());
    Some(hasher.finalize().to_hex()[..16].to_string())
}

pub fn lockfile_hashes(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for name in LOCKFILES {
        let path = root.join(name);
        if let Ok(bytes) = std::fs::read(&path) {
            out.insert(
                (*name).to_string(),
                blake3::hash(&bytes).to_hex()[..16].to_string(),
            );
        }
    }
    out
}

/// Environment variable names, sorted.
///
/// Values are never read or recorded. This is unconditional and has no setting to relax
/// it; nothing downstream may reintroduce them.
pub fn env_keys() -> Vec<String> {
    let mut keys: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    keys.sort();
    keys
}

/// `git log --oneline a..b`, structured.
pub fn commits_between(root: &Path, a: &str, b: &str) -> Vec<crate::proto::GitCommit> {
    let range = format!("{a}..{b}");
    let Some(out) = git(
        root,
        &["log", "--no-color", "--format=%H%x1f%s%x1f%an%x1f%at", &range],
    ) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|line| {
            let mut parts = line.split('\u{1f}');
            Some(crate::proto::GitCommit {
                sha: parts.next()?.to_string(),
                subject: parts.next()?.to_string(),
                author: parts.next()?.to_string(),
                at: parts.next().and_then(|s| s.parse::<i64>().ok()).map(|s| s * 1_000_000),
            })
        })
        .collect()
}

pub fn files_changed(root: &Path, a: &str, b: &str) -> Vec<String> {
    let range = format!("{a}..{b}");
    git(root, &["diff", "--name-only", &range])
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Runs a git command, returning `None` on any failure or timeout. The shim calls this on
/// the process-spawn path, so it must never stall.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Suppress credential prompts and editors: a hung prompt behind a spawn would
        // look like a freeze with no explanation.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");

    let child = cmd.spawn().ok()?;
    let out = wait_with_timeout(child, Duration::from_secs(2))?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    use std::sync::mpsc;

    let mut stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return None,
        }
    };
    let stdout = rx.recv_timeout(Duration::from_millis(200)).unwrap_or_default();
    let _ = handle.join();
    Some(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containment_beats_equality() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let nested = root.join("apps/api");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(contains(root, &nested), "monorepo subdir must resolve to the repo root");
        assert!(contains(root, root));
        assert!(!contains(&nested, root), "a parent is not inside its child");
    }

    #[test]
    fn env_values_are_never_captured() {
        std::env::set_var("LOGBEAR_TEST_SECRET", "hunter2");
        let keys = env_keys();
        assert!(keys.iter().any(|k| k == "LOGBEAR_TEST_SECRET"));
        assert!(
            !keys.iter().any(|k| k.contains("hunter2")),
            "a value leaked into the key list"
        );
    }

    #[test]
    fn project_root_of_a_plain_dir_is_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("nowhere");
        std::fs::create_dir_all(&sub).unwrap();
        // The temp dir may or may not sit inside a repo, so assert only that the root is
        // an ancestor.
        let root = project_root(&sub);
        assert!(sub.starts_with(&root) || root == sub);
    }
}

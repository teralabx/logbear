//! On-disk layout.
//!
//! ```text
//! ~/.runlog/
//!   index.db                  project registry, global run summaries, settings
//!   runlogd.sock              daemon control socket
//!   runlogd.pid
//!   runlogd.log
//!   projects/<hash>.db        one database per project
//!   spool/<uuid>.spool        shim fallback when the daemon is unreachable (C2)
//!   models/                   fastembed ONNX cache (v1.1)
//! ```

use std::path::{Path, PathBuf};

/// Root of everything runlog owns. Honours `RUNLOG_HOME`, which is how the test suite
/// keeps a run from touching the developer's real history.
pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("RUNLOG_HOME") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".runlog")
}

pub fn ensure_home() -> std::io::Result<PathBuf> {
    let h = home();
    std::fs::create_dir_all(h.join("projects"))?;
    std::fs::create_dir_all(h.join("spool"))?;
    Ok(h)
}

pub fn index_db() -> PathBuf {
    home().join("index.db")
}

pub fn socket() -> PathBuf {
    if let Some(s) = std::env::var_os("RUNLOG_SOCKET") {
        return PathBuf::from(s);
    }
    home().join("runlogd.sock")
}

pub fn pid_file() -> PathBuf {
    home().join("runlogd.pid")
}

pub fn daemon_log() -> PathBuf {
    home().join("runlogd.log")
}

pub fn spool_dir() -> PathBuf {
    home().join("spool")
}

pub fn settings_file() -> PathBuf {
    home().join("settings.json")
}

/// Stable per-project database path. Keyed on the canonical project root so a repo that
/// moves gets a new database rather than silently merging with whatever was there.
pub fn project_db(root: &Path) -> PathBuf {
    home()
        .join("projects")
        .join(format!("{}.db", project_hash(root)))
}

pub fn project_hash(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let h = blake3::hash(canonical.to_string_lossy().as_bytes());
    h.to_hex()[..16].to_string()
}

/// Where the shim spools bytes when the daemon cannot take them. Never blocks the child.
pub fn spool_file(token: &str) -> PathBuf {
    spool_dir().join(format!("{token}.spool"))
}

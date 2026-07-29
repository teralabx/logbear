//! On-disk layout.
//!
//! ```text
//! ~/.logbear/
//!   index.db                  project registry, global run summaries, settings
//!   logbeard.sock              daemon control socket
//!   logbeard.pid
//!   logbeard.log
//!   projects/<hash>.db        one database per project
//!   spool/<uuid>.spool        shim fallback when the daemon is unreachable (C2)
//!   models/                   fastembed ONNX cache (v1.1)
//! ```

use std::path::{Path, PathBuf};

/// Root of everything logbear owns. `LOGBEAR_HOME` overrides it, which is how tests avoid
/// touching real history.
pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOGBEAR_HOME") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".logbear")
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
    if let Some(s) = std::env::var_os("LOGBEAR_SOCKET") {
        return PathBuf::from(s);
    }
    home().join("logbeard.sock")
}

pub fn pid_file() -> PathBuf {
    home().join("logbeard.pid")
}

pub fn daemon_log() -> PathBuf {
    home().join("logbeard.log")
}

pub fn spool_dir() -> PathBuf {
    home().join("spool")
}

pub fn settings_file() -> PathBuf {
    home().join("settings.json")
}

/// Per-project database path, keyed on the canonical project root so a moved repo gets a
/// new database instead of merging into an existing one.
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

/// Spool file the shim writes to when the daemon cannot take bytes.
pub fn spool_file(token: &str) -> PathBuf {
    spool_dir().join(format!("{token}.spool"))
}

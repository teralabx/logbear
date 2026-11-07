//! On-disk layout.
//!
//! ```text
//! ~/.logbear/
//!   index.db                  project registry, global run summaries, settings
//!   logbeard.sock              daemon control socket
//!   logbeard.pid              the daemon's `flock`ed single-instance lock; holds its pid
//!   logbeard.log
//!   projects/<hash>.db        one database per project
//!   spool/<uuid>.spool        shim fallback when the daemon is unreachable (C2)
//!   models/                   fastembed ONNX cache (v1.1)
//! ```

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A Homebrew keg's `bin` under its version-stable `opt` link.
///
/// Anything that outlives this process must name binaries this way: a
/// `Cellar/logbear/0.1.0/bin` path is deleted by the next `brew upgrade`, silently breaking
/// every hook and service that recorded it. `opt/logbear` follows the installed version.
pub fn stable_bin_dir(dir: &Path) -> PathBuf {
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

/// The directory an installed executable should be referred to by.
///
/// On macOS `current_exe()` reports the launch path, and Homebrew puts a symlink on PATH, so
/// a brew install launched as `logbear` reports `/opt/homebrew/bin` and looks like nothing
/// in particular. The keg is only visible once the link is followed. Other symlinks are
/// left as they are: following a version manager's shim would record a path less stable
/// than the one the user chose.
pub fn install_dir(exe: &Path) -> PathBuf {
    let keg = exe
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(stable_bin_dir))
        .filter(|dir| dir.ends_with("opt/logbear/bin"));
    keg.or_else(|| exe.parent().map(stable_bin_dir))
        .unwrap_or_else(|| PathBuf::from("."))
}

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
    ensure_dirs(&h)?;
    Ok(h)
}

/// Captured logs are as sensitive as the process that wrote them, so the whole tree is
/// owner-only. An explicit mode rather than a umask, because several callers are processes
/// whose umask their children inherit.
fn ensure_dirs(home: &Path) -> std::io::Result<()> {
    for dir in [home.to_path_buf(), home.join("projects"), home.join("spool")] {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        restrict(&dir, 0o700)?;
    }
    Ok(())
}

/// Clears group and other access. Reads first so the common case writes nothing.
fn restrict(path: &Path, mode: u32) -> std::io::Result<()> {
    if std::fs::metadata(path)?.mode() & 0o777 != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Replaces a file's contents, leaving it owner-only even if it already existed.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    restrict(path, 0o600)?;
    f.write_all(bytes)
}

/// Opens the daemon's log for appending, owner-only even if it already existed.
pub fn open_private_log(path: &Path) -> std::io::Result<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    restrict(path, 0o600)?;
    Ok(f)
}

/// Makes sure a database file exists before SQLite opens it.
///
/// SQLite would create it from the process umask, and its `-wal` and `-shm` copy the
/// database's mode, so getting this one file right covers all three. An empty file is a
/// valid new database.
pub fn create_private(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    Ok(())
}

/// Brings an existing home up to [`ensure_dirs`]' standard: the files of an install from
/// before that standard existed, or created by something outside our control, such as a
/// launchd log path. Returns how many were changed.
///
/// Limited to the files logbear owns outright: `home`'s own files, `projects/` and `spool/`.
/// Anything else in the home is already behind its 0700, and may need its execute bit. A
/// symlink is never followed, since its target could be anybody's.
pub fn tighten_existing(home: &Path) -> std::io::Result<usize> {
    let mut changed = 0;
    for dir in [home.to_path_buf(), home.join("projects"), home.join("spool")] {
        for entry in std::fs::read_dir(&dir)? {
            let Ok(entry) = entry else { continue };
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_file() && meta.mode() & 0o077 != 0 {
                let private = std::fs::Permissions::from_mode(0o600);
                // A live spool can vanish between the listing and here.
                if std::fs::set_permissions(entry.path(), private).is_ok() {
                    changed += 1;
                }
            }
        }
    }
    Ok(changed)
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

/// The daemon's lock file, which doubles as the record of its pid. Held open and `flock`ed
/// for as long as the daemon runs, so "is a daemon alive?" is a lock probe rather than a
/// guess about a file that an unclean kill would have left behind either way.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o777
    }

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn directories_are_created_private() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("new");
        ensure_dirs(&home).unwrap();
        for dir in [&home, &home.join("projects"), &home.join("spool")] {
            assert_eq!(mode(dir), 0o700, "{}", dir.display());
        }
    }

    #[test]
    fn existing_directories_are_tightened() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("old");
        std::fs::create_dir_all(home.join("projects")).unwrap();
        chmod(&home, 0o755);
        chmod(&home.join("projects"), 0o775);
        ensure_dirs(&home).unwrap();
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&home.join("projects")), 0o700);
    }

    #[test]
    fn private_writes_create_owner_only_files() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("state.json");
        write_private(&file, b"{}").unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(std::fs::read(&file).unwrap(), b"{}");
    }

    #[test]
    fn private_writes_tighten_a_file_that_was_already_there() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("state.json");
        std::fs::write(&file, b"old contents").unwrap();
        chmod(&file, 0o644);
        write_private(&file, b"new").unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
    }

    #[test]
    fn the_daemon_log_is_private_and_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("logbeard.log");
        std::fs::write(&file, b"one\n").unwrap();
        chmod(&file, 0o644);
        {
            use std::io::Write;
            open_private_log(&file).unwrap().write_all(b"two\n").unwrap();
        }
        assert_eq!(mode(&file), 0o600);
        assert_eq!(std::fs::read(&file).unwrap(), b"one\ntwo\n");
    }
}

#[cfg(test)]
mod install_dir_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// Lays out a keg the way Homebrew does, with its PATH symlink.
    fn keg(root: &Path) -> PathBuf {
        let bin = root.join("Cellar/logbear/1.2.3/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(bin.join("logbear"), b"").unwrap();
        let link = root.join("bin/logbear");
        symlink(bin.join("logbear"), &link).unwrap();
        link
    }

    #[test]
    fn a_brew_install_launched_through_its_path_symlink_is_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let link = keg(&root);
        assert_eq!(install_dir(&link), root.join("opt/logbear/bin"));
    }

    #[test]
    fn a_plain_install_keeps_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("logbear");
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(install_dir(&exe), dir.path());
    }

    #[test]
    fn a_symlink_to_anything_but_a_keg_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("elsewhere");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("logbear"), b"").unwrap();
        let link = dir.path().join("logbear");
        symlink(real.join("logbear"), &link).unwrap();
        assert_eq!(install_dir(&link), dir.path());
    }
}

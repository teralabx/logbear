//! Captured logs are private by default, on a new install and on one that predates this.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon(Child);

impl Daemon {
    /// Starts a daemon and returns once it is serving. The umask is forced open so the test
    /// proves the daemon's own, rather than passing on a developer's strict shell.
    fn start(home: &Path) -> Daemon {
        let daemon = unsafe {
            Command::new(env!("CARGO_BIN_EXE_logbeard"))
                .arg("serve")
                .env("LOGBEAR_HOME", home)
                .env_remove("LOGBEAR_SOCKET")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .pre_exec(|| {
                    libc::umask(0o022);
                    Ok(())
                })
                .spawn()
                .expect("logbeard should start")
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !home.join("logbeard.sock").exists() {
            assert!(Instant::now() < deadline, "the daemon never bound its socket");
            std::thread::sleep(Duration::from_millis(10));
        }
        Daemon(daemon)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

use std::os::unix::process::CommandExt;

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Everything under `dir`, depth first.
fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, out);
        }
        out.push(path);
    }
}

#[test]
fn a_new_home_is_private_from_the_first_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _daemon = Daemon::start(&home);

    assert_eq!(mode(&home), 0o700);
    let mut all = Vec::new();
    walk(&home, &mut all);
    assert!(all.iter().any(|p| p.ends_with("index.db")), "{all:?}");
    for path in all {
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            assert_eq!(meta.mode() & 0o777, 0o700, "{}", path.display());
        } else if meta.is_file() {
            assert_eq!(meta.mode() & 0o777, 0o600, "{}", path.display());
        }
    }
}

#[test]
fn an_existing_home_is_migrated_on_start() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    for dir in ["projects", "spool", "bin"] {
        fs::create_dir_all(home.join(dir)).unwrap();
        chmod(&home.join(dir), 0o755);
    }
    chmod(&home, 0o755);

    let files = [
        "index.db",
        "index.db-wal",
        "index.db-shm",
        "settings.json",
        "logbeard.log",
        "logbeard.pid",
        "updates.json",
        "projects/abc.db",
        "projects/abc.db-wal",
        "projects/abc.db-shm",
        "spool/run.spool",
        "spool/old.spool.bad",
    ];
    for f in files {
        let body: &[u8] = if f == "settings.json" { b"{}" } else { b"" };
        fs::write(home.join(f), body).unwrap();
        chmod(&home.join(f), 0o644);
    }
    // Things that are not ours to flatten: a tool has to stay executable, and a symlink's
    // target may be anybody's.
    fs::write(home.join("bin/tool"), b"#!/bin/sh\n").unwrap();
    chmod(&home.join("bin/tool"), 0o755);
    let outside = tmp.path().join("outside");
    fs::write(&outside, b"").unwrap();
    chmod(&outside, 0o644);
    std::os::unix::fs::symlink(&outside, home.join("spool/link.spool")).unwrap();

    let _daemon = Daemon::start(&home);

    for dir in ["", "projects", "spool"] {
        assert_eq!(mode(&home.join(dir)), 0o700, "{dir:?}");
    }
    for f in files {
        // A drained or quarantined spool may have moved; what is left must be private.
        if let Ok(meta) = fs::symlink_metadata(home.join(f)) {
            assert_eq!(meta.mode() & 0o777, 0o600, "{f}");
        }
    }
    assert_eq!(mode(&home.join("bin/tool")), 0o755, "an executable lost its bit");
    assert_eq!(mode(&outside), 0o644, "a symlink was followed out of the home");
}

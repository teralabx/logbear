//! Noticing that an upgrade replaced this binary.
//!
//! `brew upgrade` and `install.sh` swap the files on disk and leave the running daemon
//! alone. A daemon keeps serving from its old inode, so without this it would go on running
//! the old version until the next reboot while every `logbear --version` reports the new
//! one. A Homebrew formula cannot fix that from `post_install`, which runs in a sandbox
//! with no access to the daemon, so the daemon watches for itself.
//!
//! It never leaves while a run is live: stopping closes open runs, and those shims would
//! finish on the spool. It waits for them, then exits non-zero. launchd and systemd restart
//! it on failure only, which is what brings the new binary up, and a daemon nothing
//! supervises is started by the next client that connects.

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

/// `EX_TEMPFAIL`: not a crash, but it does need the supervisor to start a new one.
pub const EXIT_CODE: i32 = 75;

/// Consecutive sightings before believing it, at one a tick, about five seconds. An upgrade
/// is several separate steps, and the first new file seen may not be the finished install.
const SETTLE_TICKS: u32 = 25;

/// What identifies a file across a replacement, for both a rename over it and `rm` + `cp`.
type Identity = (u64, u64);

fn identity(path: &PathBuf) -> Option<Identity> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

pub struct Watch {
    /// `None` if this process cannot tell what it was started from: then it never retires.
    origin: Option<(PathBuf, Identity)>,
    sightings: u32,
}

impl Watch {
    pub fn start() -> Watch {
        let origin = std::env::current_exe()
            .ok()
            .and_then(|path| identity(&path).map(|id| (path, id)));
        Watch {
            origin,
            sightings: 0,
        }
    }

    /// Call once a tick. True once the binary has stayed replaced and nothing is running.
    pub fn should_retire(&mut self, active_runs: usize) -> bool {
        let Some((path, original)) = &self.origin else {
            return false;
        };
        // A missing file is mid-upgrade or an uninstall. Neither has a successor to start.
        let replaced = identity(path).is_some_and(|now| now != *original);
        self.sightings = if replaced { self.sightings + 1 } else { 0 };
        self.sightings >= SETTLE_TICKS && active_runs == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watching(path: PathBuf) -> Watch {
        let id = identity(&path).unwrap();
        Watch {
            origin: Some((path, id)),
            sightings: 0,
        }
    }

    fn replace(path: &PathBuf) {
        // Written first and renamed over, so the new file cannot reuse the old inode.
        let next = path.with_extension("new");
        std::fs::write(&next, b"new").unwrap();
        std::fs::rename(&next, path).unwrap();
    }

    fn settle(watch: &mut Watch, active: usize) -> bool {
        (0..SETTLE_TICKS).fold(false, |_, _| watch.should_retire(active))
    }

    #[test]
    fn an_untouched_binary_never_retires() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("logbeard");
        std::fs::write(&exe, b"old").unwrap();
        let mut watch = watching(exe);
        assert!(!settle(&mut watch, 0));
    }

    #[test]
    fn a_replaced_binary_retires_only_after_it_settles() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("logbeard");
        std::fs::write(&exe, b"old").unwrap();
        let mut watch = watching(exe.clone());
        replace(&exe);
        assert!(!watch.should_retire(0), "one sighting is not enough");
        assert!(settle(&mut watch, 0));
    }

    #[test]
    fn a_live_run_holds_retirement_until_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("logbeard");
        std::fs::write(&exe, b"old").unwrap();
        let mut watch = watching(exe.clone());
        replace(&exe);
        assert!(!settle(&mut watch, 1));
        assert!(!watch.should_retire(1));
        assert!(watch.should_retire(0));
    }

    #[test]
    fn a_missing_binary_is_not_a_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("logbeard");
        std::fs::write(&exe, b"old").unwrap();
        let mut watch = watching(exe.clone());
        std::fs::remove_file(&exe).unwrap();
        assert!(!settle(&mut watch, 0), "uninstalled, or mid-upgrade");
    }
}

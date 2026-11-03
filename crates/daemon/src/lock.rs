//! The single-instance guard.
//!
//! A socket file cannot answer "is another daemon alive?". Its presence proves only that
//! *something* bound it once, and probing it conflates "nobody is home" with "the owner was
//! too busy to answer" — so the loser of that guess deletes a live daemon's socket and binds
//! its own, leaving two processes writing the same databases.
//!
//! An advisory `flock` answers the question outright. The kernel releases it when the fd
//! closes, which includes `SIGKILL`, panics and power loss, so unlike a pid file it cannot go
//! stale: holding the lock *is* being the daemon.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Proof that this process is the only daemon. Lives as long as `serve` does.
pub struct InstanceLock {
    file: File,
}

impl InstanceLock {
    /// `Ok(None)` means another daemon holds it — a duplicate start, not an error.
    ///
    /// The file is never unlinked, by us or on drop: a lock is on an inode, so removing the
    /// path would let the next starter create a *different* file and lock that instead,
    /// reintroducing the very race this replaces. It is truncated and rewritten in place.
    pub fn acquire(path: &Path) -> std::io::Result<Option<InstanceLock>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;

        // Non-blocking: a duplicate start should report the incumbent and leave, not queue
        // up behind it and silently become the daemon whenever that one stops.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            return match e.kind() {
                std::io::ErrorKind::WouldBlock => Ok(None),
                _ => Err(e),
            };
        }

        let mut lock = InstanceLock { file };
        lock.write_pid(std::process::id())?;
        Ok(Some(lock))
    }

    /// The pid recorded by whoever holds the lock, for a message worth reading.
    pub fn holder(path: &Path) -> Option<u32> {
        let mut s = String::new();
        File::open(path).ok()?.read_to_string(&mut s).ok()?;
        s.trim().parse().ok()
    }

    fn write_pid(&mut self, pid: u32) -> std::io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        write!(self.file, "{pid}")?;
        self.file.flush()
    }

    /// Blanks the record on a clean stop, so the file never names a process that has exited.
    /// The lock itself outlives this call and is released by the kernel at process exit.
    pub fn release(mut self) {
        let _ = self.file.set_len(0);
        let _ = self.file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_holder_excludes_everyone_else_and_records_its_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("logbeard.pid");

        let held = InstanceLock::acquire(&path).unwrap().expect("first wins");
        assert_eq!(InstanceLock::holder(&path), Some(std::process::id()));

        // flock conflicts across open file descriptions, so this is the same answer another
        // process would get.
        assert!(
            InstanceLock::acquire(&path).unwrap().is_none(),
            "a second daemon must not be able to take the lock"
        );

        drop(held);
        assert!(
            InstanceLock::acquire(&path).unwrap().is_some(),
            "the lock must be reusable once the holder lets go"
        );
    }

    #[test]
    fn a_lock_file_left_by_a_dead_daemon_is_not_treated_as_held() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("logbeard.pid");
        // What an unclean kill leaves behind: the file, naming a pid that is long gone.
        std::fs::write(&path, "424242").unwrap();

        let lock = InstanceLock::acquire(&path).unwrap();
        assert!(lock.is_some(), "a stale file must not block a fresh start");
        assert_eq!(InstanceLock::holder(&path), Some(std::process::id()));
    }

    #[test]
    fn releasing_clears_the_pid_so_nothing_reads_it_as_live() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("logbeard.pid");

        InstanceLock::acquire(&path).unwrap().unwrap().release();

        assert_eq!(InstanceLock::holder(&path), None);
        assert!(path.exists(), "the lock file itself must survive");
    }
}

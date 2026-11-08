//! Checkpointing, off the capture path.
//!
//! Left to itself SQLite checkpoints inside whichever commit crosses the threshold, and a
//! checkpoint is mostly `fsync`: profiling put 44% of the ingest thread there. So
//! auto-checkpoint is off on the writer, and the work moves to a thread with its own
//! connection, where a slow disk costs nobody anything.
//!
//! A passive checkpoint alone cannot keep up with a sustained writer. The log only restarts
//! once every frame is backfilled, and frames keep arriving while the checkpoint runs, so
//! the file would grow for as long as the writer does. Two things stop that:
//!
//! * Each pass ends by asking the writer to restart the log, which it does on its next
//!   commit. The passive passes before it have done nearly all the work, so the stall is
//!   short. The writer does it, rather than the thread taking the writer's mutex, because
//!   `std`'s mutex is not fair on Linux: a writer that commits back to back takes the lock
//!   straight back every time, and the thread waited there for as long as the writes went on.
//! * Past [`WAL_HARD_PAGES`] the writer stops and checkpoints for itself. That is the
//!   backstop for a reader pinning the log, and it is a stall by design: an unbounded WAL
//!   is worse, since it would count against retention and eat the disk.

use crate::store::Connection;
use rusqlite::hooks::Wal;
use std::cell::Cell;
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, TryLockError};
use std::time::Duration;

/// Log size at which the background thread is woken: 32 MB at the 4 KB page size.
pub const WAL_SOFT_PAGES: i32 = 8 * 1024;
/// Log size at which the writer checkpoints for itself: 128 MB.
pub const WAL_HARD_PAGES: i32 = 32 * 1024;
/// What a reset log may keep on disk.
pub const JOURNAL_SIZE_LIMIT: i64 = 32 * 1024 * 1024;

/// How long the writer's restart after a pass may wait for readers.
const RESTART_WAIT: Duration = Duration::from_millis(100);
/// Frames still unbackfilled at which it is worth stopping the writer to finish the job.
const RESTART_WHEN_BEHIND_PAGES: i64 = 512;
const MAX_PASSIVE_PASSES: usize = 4;
/// How long a checkpoint thread lingers without work. An idle project keeps no thread and
/// no connection.
const IDLE_EXIT: Duration = Duration::from_secs(5);
/// How long the writer's own checkpoint waits for readers.
const BLOCKING_WAIT: Duration = Duration::from_millis(100);
/// Spacing of the writer's own attempts, so a pinned log costs one stall a second rather
/// than one on every commit.
const BLOCKING_RETRY_MICROS: i64 = 1_000_000;

thread_local! {
    /// Set by the WAL hook, which runs on the committing thread and which rusqlite gives
    /// no state of its own: a bare `fn`, no closure.
    static WAL_PAGES: Cell<i32> = const { Cell::new(0) };
}

/// Replaces auto-checkpoint with a note of how long the log is.
///
/// The order matters: `wal_autocheckpoint` is itself built on the WAL hook, and setting it
/// after this would remove the hook.
pub fn install(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA wal_autocheckpoint = 0; PRAGMA journal_size_limit = {JOURNAL_SIZE_LIMIT};"
    ))?;
    conn.wal_hook(Some(note_pages));
    Ok(())
}

fn note_pages(_: &Wal, pages: c_int) -> rusqlite::Result<()> {
    WAL_PAGES.with(|p| p.set(pages));
    Ok(())
}

/// Log length reported by this thread's most recent commit, cleared by reading.
fn take_pages() -> i32 {
    WAL_PAGES.with(|p| p.replace(0))
}

#[derive(Default)]
struct State {
    wanted: bool,
    running: bool,
    stop: bool,
}

struct Shared {
    path: PathBuf,
    state: Mutex<State>,
    wake: Condvar,
    /// Held by whoever is checkpointing. SQLite runs one checkpoint at a time and turns a
    /// second away at once with busy, without waiting, so the writer and the thread take
    /// turns here rather than failing each other.
    checkpointing: Mutex<()>,
    /// Set when a pass has backfilled the log, so the writer's next commit restarts it. No
    /// new pass starts until the writer has.
    restart_ready: AtomicBool,
    /// Passes finished, for tests.
    passes: AtomicUsize,
}

/// One per open project database, shared by every clone of its [`Db`](super::Db).
pub struct Checkpointer {
    shared: Arc<Shared>,
    next_blocking_at: Mutex<i64>,
}

impl Checkpointer {
    pub fn new(path: &Path) -> Checkpointer {
        Checkpointer {
            shared: Arc::new(Shared {
                path: path.to_path_buf(),
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
                checkpointing: Mutex::new(()),
                restart_ready: AtomicBool::new(false),
                passes: AtomicUsize::new(0),
            }),
            next_blocking_at: Mutex::new(0),
        }
    }

    /// Called after every write with the writer's mutex held.
    pub fn after_write(&self, conn: &Connection) {
        let pages = take_pages();
        if self.shared.restart_ready.load(Ordering::Acquire) {
            // Never waits: a pass that is still running gets the next commit instead.
            let held = match self.shared.checkpointing.try_lock() {
                Ok(guard) => Some(guard),
                Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
                Err(TryLockError::WouldBlock) => None,
            };
            if let Some(_held) = held {
                self.shared.restart_ready.store(false, Ordering::Release);
                // A restart reuses the file rather than shrinking it, so past the hard limit
                // the truncate below still runs. Below it, the log now starts over and the
                // length noted before no longer applies.
                if restart(conn) && pages < WAL_HARD_PAGES {
                    return;
                }
            }
        }
        if pages >= WAL_SOFT_PAGES {
            self.request();
        }
        if pages >= WAL_HARD_PAGES {
            self.checkpoint_blocking(conn);
        }
    }

    fn checkpoint_blocking(&self, conn: &Connection) {
        let now = crate::now_micros();
        let mut next = self
            .next_blocking_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if now < *next {
            return;
        }
        *next = now + BLOCKING_RETRY_MICROS;
        drop(next);
        // Best effort, and brief: capture is stopped for as long as this waits. A reader that
        // outlasts it leaves the log as it is, and the next attempt comes round in a second.
        let _held = self
            .shared
            .checkpointing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _ = conn.busy_timeout(BLOCKING_WAIT);
        let _ = checkpoint(conn, "TRUNCATE");
        let _ = conn.busy_timeout(crate::store::BUSY_TIMEOUT);
    }

    fn request(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.wanted = true;
        if state.running {
            self.shared.wake.notify_one();
            return;
        }
        state.running = true;
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("logbear-checkpoint".into())
            .spawn(move || run(shared));
        // Out of threads: the hard limit still bounds the log.
        if spawned.is_err() {
            state.running = false;
        }
    }
}

impl Drop for Checkpointer {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stop = true;
        self.shared.wake.notify_one();
    }
}

fn run(shared: Arc<Shared>) {
    let Ok(conn) = open(&shared.path) else {
        shared.state.lock().unwrap_or_else(PoisonError::into_inner).running = false;
        return;
    };
    loop {
        {
            let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            while !state.wanted && !state.stop {
                let (next, timeout) = shared
                    .wake
                    .wait_timeout(state, IDLE_EXIT)
                    .unwrap_or_else(PoisonError::into_inner);
                state = next;
                if timeout.timed_out() && !state.wanted {
                    state.stop = true;
                }
            }
            if state.stop {
                state.running = false;
                return;
            }
            state.wanted = false;
        }
        pass(&conn, &shared);
    }
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch(&format!(
        "PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = {};",
        RESTART_WAIT.as_millis()
    ))?;
    Ok(conn)
}

/// Backfills without stopping anyone, then hands the restart to the writer.
///
/// Never takes the writer's mutex. See the module docs for why.
fn pass(conn: &Connection, shared: &Shared) {
    // The writer has not restarted since the last pass. It will on its next commit, and
    // another pass now would only take the checkpoint from under it.
    if shared.restart_ready.load(Ordering::Acquire) {
        return;
    }
    {
        let _held = shared.checkpointing.lock().unwrap_or_else(PoisonError::into_inner);
        for _ in 0..MAX_PASSIVE_PASSES {
            match checkpoint(conn, "PASSIVE") {
                Ok(c) if c.log_pages - c.backfilled_pages <= RESTART_WHEN_BEHIND_PAGES => break,
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }
    shared.restart_ready.store(true, Ordering::Release);
    shared.passes.fetch_add(1, Ordering::Relaxed);
}

/// Restarts the log from the writer, which holds its own mutex, so no frame arrives while it
/// runs. Only the frames committed since the last pass are left to backfill. A reader still
/// on the log fails this with busy, and the next pass asks again. Whether it restarted.
fn restart(conn: &Connection) -> bool {
    let _ = conn.busy_timeout(RESTART_WAIT);
    let done = matches!(checkpoint(conn, "RESTART"), Ok(c) if !c.busy);
    let _ = conn.busy_timeout(crate::store::BUSY_TIMEOUT);
    done
}

/// What a checkpoint managed.
pub struct Checkpoint {
    /// It could not finish, because something else held the log.
    pub busy: bool,
    pub log_pages: i64,
    pub backfilled_pages: i64,
}

pub fn checkpoint(conn: &Connection, mode: &str) -> rusqlite::Result<Checkpoint> {
    conn.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |r| {
        Ok(Checkpoint {
            busy: r.get::<_, i64>(0)? != 0,
            log_pages: r.get(1)?,
            backfilled_pages: r.get(2)?,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Db;

    fn wal_len(path: &Path) -> u64 {
        std::fs::metadata(format!("{}-wal", path.display())).map(|m| m.len()).unwrap_or(0)
    }

    /// 2 MB a commit, with the writer busy the whole time, which is the case a passive
    /// checkpoint alone cannot keep up with.
    fn hammer(db: &Db, commits: usize) -> u64 {
        db.with(|c| {
            c.execute_batch("CREATE TABLE t(x BLOB);")?;
            Ok(())
        })
        .unwrap();
        let blob = vec![7u8; 2 * 1024 * 1024];
        let mut peak = 0;
        for _ in 0..commits {
            db.with(|c| {
                c.execute("INSERT INTO t VALUES (?1)", [&blob])?;
                Ok(())
            })
            .unwrap();
            peak = peak.max(wal_len(db.path()));
        }
        peak
    }

    #[test]
    fn the_wal_stays_bounded_under_sustained_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("p.db"), 1).unwrap();
        let peak = hammer(&db, 200);
        assert!(
            peak <= (WAL_HARD_PAGES as u64 + 2048) * 4096,
            "400 MB written and the log reached {peak} bytes"
        );
    }

    /// The Linux failure, forced on every platform: the writer never lets go of its mutex,
    /// as a writer committing back to back on an unfair lock never effectively does. The
    /// checkpoint thread used to wait on that mutex, so it finished nothing and the log grew
    /// to twice the hard limit. It must get its work done without the lock.
    #[test]
    fn the_log_is_restarted_while_the_writer_never_lets_go() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("p.db"), 1).unwrap();
        let conn = db.writer();
        conn.execute_batch("CREATE TABLE t(x BLOB);").unwrap();
        let blob = vec![7u8; 2 * 1024 * 1024];
        let mut peak = 0;
        for _ in 0..200 {
            conn.execute("INSERT INTO t VALUES (?1)", [&blob]).unwrap();
            db.checkpointer.after_write(&conn);
            peak = peak.max(wal_len(db.path()));
        }
        let passes = db.checkpointer.shared.passes.load(Ordering::Relaxed);
        assert!(passes > 0, "the checkpoint thread finished no pass while the writer held its lock");
        assert!(
            peak <= (WAL_HARD_PAGES as u64 + 2048) * 4096,
            "400 MB written with the writer's lock held and the log reached {peak} bytes"
        );
    }

    #[test]
    fn a_reader_pinning_the_log_cannot_stop_writes_or_escape_the_writers_own_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p.db");
        let db = Db::open(&path, 1).unwrap();
        hammer(&db, 1);

        // A read transaction held open keeps every later frame in the log.
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN; SELECT count(*) FROM meta;").unwrap();
        let blob = vec![7u8; 4 * 1024 * 1024];
        for _ in 0..40 {
            db.with(|c| {
                c.execute("INSERT INTO t VALUES (?1)", [&blob])?;
                Ok(())
            })
            .unwrap();
        }
        assert!(wal_len(&path) > (WAL_HARD_PAGES as u64) * 4096, "setup: the log is pinned");

        // Once it lets go, the next commit past the limit brings the log back down.
        reader.execute_batch("COMMIT;").unwrap();
        std::thread::sleep(std::time::Duration::from_micros(BLOCKING_RETRY_MICROS as u64));
        db.with(|c| {
            c.execute("INSERT INTO t VALUES (?1)", [&blob])?;
            Ok(())
        })
        .unwrap();
        assert!(
            wal_len(&path) < (WAL_HARD_PAGES as u64) * 4096,
            "the log stayed at {} bytes",
            wal_len(&path)
        );
    }

    #[test]
    fn the_checkpoint_thread_goes_away_when_the_database_does() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p.db");
        let db = Db::open(&path, 1).unwrap();
        hammer(&db, 20);
        let shared = db.checkpointer_shared_for_test();
        drop(db);
        let deadline = std::time::Instant::now() + IDLE_EXIT * 2;
        while Arc::strong_count(&shared) > 1 {
            assert!(std::time::Instant::now() < deadline, "the thread outlived its database");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    impl Checkpointer {
        pub(crate) fn shared_for_test(&self) -> Arc<impl Sized> {
            self.shared.clone()
        }
    }
}

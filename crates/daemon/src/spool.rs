//! Backfill of spool files.
//!
//! When the daemon is down, upgrading or wedged, the shim writes captured bytes to a spool
//! file and carries on. On next contact the spool becomes a real run, in order, with its
//! original timestamps.
//!
//! A shim that gave up on the daemon partway through a run marks its spool with
//! `continues_run`, and the spool is appended to that run rather than starting a new one.

use crate::state::{Daemon, LiveRun, ProjectHandle};
use logbear_core::proto::{Frame, SpoolHeader, SPOOL_VERSION};
use logbear_core::Result;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How long a temporary spool may sit before it is treated as abandoned. A shim holds one
/// for a moment while it writes the header.
const STALE_TEMPORARY_SECS: u64 = 3600;

fn read_header(reader: &mut impl BufRead) -> Result<SpoolHeader> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(serde_json::from_str(line.trim())?)
}

/// Feeds every frame to the run, returning the bytes replayed.
///
/// A file ends mid-frame when the writer lost power, was killed or ran out of disk. What was
/// written before that is good, and it is the only copy, so a torn tail ends the replay
/// rather than failing it.
fn replay(
    daemon: &Arc<Daemon>,
    handle: &Arc<ProjectHandle>,
    live: &Arc<LiveRun>,
    reader: &mut impl Read,
    path: &Path,
) -> usize {
    let mut replayed = 0usize;
    loop {
        match Frame::read_from(reader) {
            Ok(Some(Frame::Data { captured_at, bytes })) => {
                replayed += bytes.len();
                // A single corrupt frame should not cost the whole run.
                let _ = daemon.ingest(handle, live, &bytes, captured_at);
            }
            Ok(Some(Frame::Json(_))) => {}
            Ok(None) => break,
            Err(e) => {
                eprintln!("logbeard: {} ends early ({e}); kept what was there", path.display());
                break;
            }
        }
    }
    replayed
}

/// Ingests one spool file and deletes it, returning the number of bytes replayed.
pub fn ingest(daemon: &Arc<Daemon>, path: &Path) -> Result<usize> {
    let file = std::fs::File::open(path).map_err(|e| logbear_core::Error::io(path, e))?;
    let mut reader = BufReader::new(file);
    let header = read_header(&mut reader)?;

    let continued = match header.extra.get("continues_run").and_then(|id| id.parse().ok()) {
        Some(run_id) => daemon.resume_run(&header.stamp, header.started_at, run_id)?,
        None => None,
    };
    // A run deleted or evicted since, one whose id now belongs to another run, or one that
    // ended with an outcome after all cannot take the tail; it becomes a run of its own as
    // if it had never been connected.
    let (handle, live) = match continued {
        Some(pair) => pair,
        None => daemon.start_run(&header.stamp)?,
    };
    // Frames the shim could not keep up with are gone; the schema has nowhere to put the
    // count on the run, so it goes to the daemon log rather than vanishing silently.
    if let Some(n) = header.extra.get("dropped_chunks") {
        eprintln!(
            "logbeard: run {} lost {n} chunk(s) the shim could not keep up with",
            live.run_id
        );
    }

    let replayed = replay(daemon, &handle, &live, &mut reader, path);

    daemon.finish_run(
        &handle,
        &live,
        header.exit_code,
        header.signal.as_deref(),
    )?;

    // Only now, once the run is durable, is it safe to lose the spool.
    let _ = std::fs::remove_file(path);
    Ok(replayed)
}

/// Replays a segment into a run that is live on its own connection, then deletes it.
///
/// The path comes from the wire, so anything outside the spool directory is refused. A
/// segment that is already gone was dropped by the shim to stay within its disk budget.
pub fn replay_segment(
    daemon: &Arc<Daemon>,
    handle: &Arc<ProjectHandle>,
    live: &Arc<LiveRun>,
    path: &Path,
) {
    if path.parent() != Some(logbear_core::paths::spool_dir().as_path()) {
        eprintln!("logbeard: refused a segment outside the spool: {}", path.display());
        return;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    match read_header(&mut reader) {
        Ok(_) => {
            replay(daemon, handle, live, &mut reader, path);
            let _ = std::fs::remove_file(path);
        }
        Err(e) => quarantine(path, &e),
    }
}

fn quarantine(path: &Path, why: &logbear_core::Error) {
    // Moved aside rather than retried forever or deleted: it is the only copy of that output.
    eprintln!("logbeard: could not ingest {}: {why}", path.display());
    let _ = std::fs::rename(path, path.with_extension("spool.bad"));
}

/// What a sweep should do with a file, decided without touching its contents.
#[derive(Debug, PartialEq)]
enum Plan {
    Ingest,
    /// Not ours to touch yet: a live shim is writing it, the run is still streaming, or it
    /// came from a newer version.
    Leave,
}

fn plan_for(daemon: &Arc<Daemon>, path: &Path) -> Plan {
    let Ok(file) = std::fs::File::open(path) else {
        return Plan::Leave;
    };
    let mut reader = BufReader::new(file);
    let Ok(header) = read_header(&mut reader) else {
        // Unreadable: the sweep quarantines it through the normal path.
        return Plan::Ingest;
    };
    if header.version > SPOOL_VERSION {
        return Plan::Leave;
    }
    // Its marker may still be in that connection's socket. Replaying here could put the
    // bytes ahead of what the socket holds.
    let live = header
        .extra
        .get("continues_run")
        .and_then(|id| id.parse().ok())
        .is_some_and(|id| daemon.is_live(&header.stamp, id));
    if live {
        Plan::Leave
    } else {
        Plan::Ingest
    }
}

/// Holds the file's lock for as long as it lives; `None` while a shim holds it.
struct Claim {
    _file: std::fs::File,
}

impl Claim {
    fn take(path: &Path) -> Option<Claim> {
        let file = std::fs::File::open(path).ok()?;
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        locked.then_some(Claim { _file: file })
    }
}

fn spool_files(extension: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(logbear_core::paths::spool_dir()) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == extension))
        .collect();
    files.sort();
    files
}

fn remove_stale_temporaries() {
    for path in spool_files("tmp") {
        let old = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > STALE_TEMPORARY_SECS);
        if old {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Ingests every finished spool file, oldest first. The daemon runs this on a timer, so a
/// run that could not reach it appears on its own rather than waiting for the next command.
pub fn drain_all(daemon: &Arc<Daemon>) -> usize {
    remove_stale_temporaries();
    let mut done = 0;
    for path in spool_files("spool") {
        let Some(_claim) = Claim::take(&path) else {
            continue;
        };
        if plan_for(daemon, &path) == Plan::Leave {
            continue;
        }
        match ingest(daemon, &path) {
            Ok(_) => done += 1,
            Err(e) => quarantine(&path, &e),
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use logbear_core::model::{RunId, RunStamp};
    use std::io::Write;
    use std::os::fd::AsRawFd;

    #[test]
    fn a_spooled_run_becomes_a_real_run() {
        let _guard = crate::state::tests::HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();

        let work = tmp.path().join("proj");
        std::fs::create_dir_all(&work).unwrap();
        let path = logbear_core::paths::spool_dir().join("t.spool");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        {
            let mut f = std::fs::File::create(&path).unwrap();
            let header = SpoolHeader {
                version: 1,
                stamp: RunStamp {
                    command: vec!["mix".into(), "phx.server".into()],
                    cwd: work.to_string_lossy().into_owned(),
                    project_name: "proj".into(),
                    ..Default::default()
                },
                started_at: logbear_core::now_micros(),
                exit_code: Some(1),
                signal: None,
                extra: Default::default(),
            };
            writeln!(f, "{}", serde_json::to_string(&header).unwrap()).unwrap();
            Frame::Data {
                captured_at: logbear_core::now_micros(),
                bytes: b"Error: spooled while the daemon was down\n".to_vec(),
            }
            .write_to(&mut f)
            .unwrap();
        }

        let n = drain_all(&daemon);
        assert_eq!(n, 1);
        assert!(!path.exists(), "the spool should be gone once the run is durable");

        let handle = daemon
            .resolve(&logbear_core::proto::ProjectRef::Path(
                work.to_string_lossy().into_owned(),
            ))
            .unwrap();
        let runs = logbear_core::store::read::list_runs(&handle.db, 10, 0).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].exit_code, Some(1));
        assert_eq!(runs[0].counters.n_error, 1);
    }

    /// Writes a spool holding `lines` as one data frame, as a shim that gave up on run
    /// `continues` would.
    fn write_spool(path: &Path, work: &Path, continues: Option<RunId>, lines: &str) {
        let mut extra = std::collections::BTreeMap::new();
        if let Some(id) = continues {
            extra.insert("continues_run".to_string(), id.to_string());
        }
        extra.insert("dropped_chunks".to_string(), "3".to_string());
        let header = SpoolHeader {
            version: 1,
            stamp: RunStamp {
                command: vec!["mix".into(), "phx.server".into()],
                cwd: work.to_string_lossy().into_owned(),
                project_name: "proj".into(),
                ..Default::default()
            },
            started_at: logbear_core::now_micros(),
            exit_code: Some(3),
            signal: None,
            extra,
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(path).unwrap();
        writeln!(f, "{}", serde_json::to_string(&header).unwrap()).unwrap();
        Frame::Data {
            captured_at: logbear_core::now_micros(),
            bytes: lines.as_bytes().to_vec(),
        }
        .write_to(&mut f)
        .unwrap();
    }

    fn numbered(range: std::ops::Range<usize>) -> String {
        range.map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn a_spool_that_continues_a_run_lands_in_that_run() {
        let _guard = crate::state::tests::HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();
        let work = tmp.path().join("proj");
        std::fs::create_dir_all(&work).unwrap();

        // The shim starts a run over the socket and streams the first half of it.
        let stamp = RunStamp {
            command: vec!["mix".into(), "phx.server".into()],
            cwd: work.to_string_lossy().into_owned(),
            project_name: "proj".into(),
            ..Default::default()
        };
        let (handle, live) = daemon.start_run(&stamp).unwrap();
        let now = logbear_core::now_micros();
        daemon.ingest(&handle, &live, numbered(0..150).as_bytes(), now).unwrap();
        daemon.ingest(&handle, &live, b"Error: before the drop\n", now).unwrap();

        // It then gives up on the socket. The connection handler sees EOF and closes the
        // run as an unexpected death, exactly as `conn::serve` does.
        daemon.finish_run(&handle, &live, None, None).unwrap();

        // The rest of the run, spooled, is drained later.
        let path = logbear_core::paths::spool_dir().join("c.spool");
        let tail = format!("{}Error: after the drop\n", numbered(151..300));
        write_spool(&path, &work, Some(live.run_id), &tail);
        assert_eq!(drain_all(&daemon), 1);
        assert!(!path.exists());

        let runs = logbear_core::store::read::list_runs(&handle.db, 10, 0).unwrap();
        assert_eq!(runs.len(), 1, "the spool split the run in two");
        let run = &runs[0];
        assert_eq!(run.id, live.run_id);
        assert_eq!(run.exit_code, Some(3), "the spool's outcome was not recorded");
        assert!(run.ended_at.is_some());
        assert_eq!(run.counters.n_error, 2, "counters restarted from zero");

        // Every line, in order, at consecutive indices.
        let raw = handle
            .db
            .with(|c| logbear_core::store::raw_range(c, run.id, 0, 1_000))
            .unwrap();
        let text = String::from_utf8(raw).unwrap();
        let got: Vec<&str> = text.lines().collect();
        let mut want: Vec<String> = (0..150).map(|i| format!("line {i}")).collect();
        want.push("Error: before the drop".into());
        want.extend((151..300).map(|i| format!("line {i}")));
        want.push("Error: after the drop".into());
        assert_eq!(got, want);

        let chunks: Vec<(i64, i64, i64)> = handle
            .db
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT seq, first_line, n_lines FROM chunks WHERE run_id = ?1 ORDER BY seq",
                )?;
                let rows = stmt.query_map([run.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .unwrap();
        let mut next_line = 0;
        for (i, (seq, first_line, n_lines)) in chunks.iter().enumerate() {
            assert_eq!(*seq, i as i64, "chunk sequence has a gap: {chunks:?}");
            assert_eq!(*first_line, next_line, "line indices have a gap: {chunks:?}");
            next_line += n_lines;
        }
        assert_eq!(next_line, want.len() as i64);
    }

    #[test]
    fn a_spool_whose_run_is_gone_becomes_a_run_of_its_own() {
        let _guard = crate::state::tests::HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();
        let work = tmp.path().join("proj");
        std::fs::create_dir_all(&work).unwrap();

        let stamp = RunStamp {
            command: vec!["mix".into(), "phx.server".into()],
            cwd: work.to_string_lossy().into_owned(),
            project_name: "proj".into(),
            ..Default::default()
        };
        let (handle, live) = daemon.start_run(&stamp).unwrap();
        daemon.ingest(&handle, &live, b"first half\n", logbear_core::now_micros()).unwrap();
        daemon.finish_run(&handle, &live, None, None).unwrap();
        logbear_core::store::retention::delete_run(&handle.db, live.run_id).unwrap();

        // SQLite hands the freed rowid to the next run, which is a different command that
        // also died without an outcome. The spool must not be appended to it.
        let mut other = stamp.clone();
        other.command = vec!["npm".into(), "run".into(), "dev".into()];
        let (h2, reused) = daemon.start_run(&other).unwrap();
        assert_eq!(reused.run_id, live.run_id, "setup: the rowid was not reused");
        daemon.finish_run(&h2, &reused, None, None).unwrap();

        let path = logbear_core::paths::spool_dir().join("g.spool");
        write_spool(&path, &work, Some(live.run_id), "second half\n");
        assert_eq!(drain_all(&daemon), 1);

        let runs = logbear_core::store::read::list_runs(&handle.db, 10, 0).unwrap();
        assert_eq!(runs.len(), 2);
        let impostor = runs.iter().find(|r| r.id == reused.run_id).unwrap();
        assert_eq!(impostor.exit_code, None, "the spool was appended to an unrelated run");
        let spooled = runs.iter().find(|r| r.id != reused.run_id).unwrap();
        assert_eq!(spooled.exit_code, Some(3));
        assert_eq!(spooled.command[0], "mix");
    }

    fn work_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        let work = tmp.path().join("proj");
        std::fs::create_dir_all(&work).unwrap();
        work
    }

    #[test]
    fn a_torn_tail_keeps_the_output_and_the_run() {
        let _guard = crate::state::tests::HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();
        let work = work_dir(&tmp);
        let path = logbear_core::paths::spool_dir().join("torn.spool");
        write_spool(&path, &work, None, "Error: before the tear\n");

        // A frame header promising far more than the file holds.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[2, 0, 16, 0, 0, 1, 2, 3]).unwrap();
        drop(f);

        assert_eq!(drain_all(&daemon), 1);
        assert!(!path.exists());
        assert!(!path.with_extension("spool.bad").exists(), "good output was quarantined");
        let handle = daemon
            .resolve(&logbear_core::proto::ProjectRef::Path(work.to_string_lossy().into_owned()))
            .unwrap();
        let runs = logbear_core::store::read::list_runs(&handle.db, 10, 0).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].exit_code, Some(3));
        assert_eq!(runs[0].counters.n_error, 1);
    }

    #[test]
    fn a_spool_held_by_a_live_shim_is_left_alone() {
        let _guard = crate::state::tests::HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();
        let work = work_dir(&tmp);
        let path = logbear_core::paths::spool_dir().join("live.spool");
        write_spool(&path, &work, None, "still being written\n");

        let held = std::fs::File::open(&path).unwrap();
        let rc = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) };
        assert_eq!(rc, 0);
        assert_eq!(drain_all(&daemon), 0);
        assert!(path.exists(), "a live spool was taken");

        drop(held);
        assert_eq!(drain_all(&daemon), 1, "the sweep never came back for it");
    }

    #[test]
    fn a_spool_for_a_run_still_streaming_waits_for_that_run() {
        let _guard = crate::state::tests::HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();
        let work = work_dir(&tmp);
        let stamp = RunStamp {
            command: vec!["mix".into()],
            cwd: work.to_string_lossy().into_owned(),
            project_name: "proj".into(),
            ..Default::default()
        };
        let (handle, live) = daemon.start_run(&stamp).unwrap();
        let path = logbear_core::paths::spool_dir().join("s.spool");
        write_spool(&path, &work, Some(live.run_id), "tail\n");

        assert_eq!(drain_all(&daemon), 0);
        assert!(path.exists());

        daemon.finish_run(&handle, &live, None, None).unwrap();
        assert_eq!(drain_all(&daemon), 1);
    }

    #[test]
    fn a_spool_from_a_newer_version_is_left_for_that_version() {
        let _guard = crate::state::tests::HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();
        let path = logbear_core::paths::spool_dir().join("new.spool");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = serde_json::json!({
            "version": SPOOL_VERSION + 1, "stamp": RunStamp::default(), "started_at": 0,
            "exit_code": null, "signal": null, "extra": {},
        });
        std::fs::write(&path, format!("{header}\n")).unwrap();

        assert_eq!(drain_all(&daemon), 0);
        assert!(path.exists());
        assert!(!path.with_extension("spool.bad").exists());
    }

    #[test]
    fn a_segment_replays_into_the_live_run_in_order_and_is_deleted() {
        let _guard = crate::state::tests::HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();
        let work = work_dir(&tmp);
        let stamp = RunStamp {
            command: vec!["mix".into()],
            cwd: work.to_string_lossy().into_owned(),
            project_name: "proj".into(),
            ..Default::default()
        };
        let (handle, live) = daemon.start_run(&stamp).unwrap();
        let now = logbear_core::now_micros();
        daemon.ingest(&handle, &live, b"one\n", now).unwrap();

        let path = logbear_core::paths::spool_dir().join("r-0001.spool");
        write_spool(&path, &work, Some(live.run_id), "two\n");
        replay_segment(&daemon, &handle, &live, &path);
        assert!(!path.exists());

        daemon.ingest(&handle, &live, b"three\n", now).unwrap();
        daemon.finish_run(&handle, &live, Some(0), None).unwrap();

        let raw = handle
            .db
            .with(|c| logbear_core::store::raw_range(c, live.run_id, 0, 100))
            .unwrap();
        assert_eq!(String::from_utf8(raw).unwrap(), "one\r\ntwo\r\nthree\r\n");
    }

    #[test]
    fn a_segment_path_from_outside_the_spool_is_refused() {
        let _guard = crate::state::tests::HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();
        let work = work_dir(&tmp);
        let elsewhere = tmp.path().join("elsewhere.spool");
        write_spool(&elsewhere, &work, None, "not yours\n");
        let stamp = RunStamp {
            command: vec!["mix".into()],
            cwd: work.to_string_lossy().into_owned(),
            project_name: "proj".into(),
            ..Default::default()
        };
        let (handle, live) = daemon.start_run(&stamp).unwrap();
        replay_segment(&daemon, &handle, &live, &elsewhere);
        assert!(elsewhere.exists(), "a client got the daemon to delete a file of its choosing");
    }

    #[test]
    fn an_unreadable_spool_is_set_aside_not_deleted() {
        let _guard = crate::state::tests::HOME_LOCK.lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();
        let path = logbear_core::paths::spool_dir().join("broken.spool");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"this is not a spool file\n").unwrap();

        drain_all(&daemon);
        assert!(!path.exists());
        assert!(
            path.with_extension("spool.bad").exists(),
            "the only copy of that output was deleted"
        );
    }
}

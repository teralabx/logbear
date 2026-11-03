//! Backfill of spool files.
//!
//! When the daemon is down, upgrading or wedged, the shim writes captured bytes to a spool
//! file and carries on. On next contact the spool becomes a real run, in order, with its
//! original timestamps.
//!
//! A shim that gave up on the daemon partway through a run marks its spool with
//! `continues_run`, and the spool is appended to that run rather than starting a new one.

use crate::state::Daemon;
use logbear_core::proto::{Frame, SpoolHeader};
use logbear_core::Result;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

/// Ingests one spool file and deletes it, returning the number of bytes replayed.
pub fn ingest(daemon: &Arc<Daemon>, path: &Path) -> Result<usize> {
    let file = std::fs::File::open(path).map_err(|e| logbear_core::Error::io(path, e))?;
    let mut reader = BufReader::new(file);

    let mut header_line = String::new();
    reader.read_line(&mut header_line)?;
    let header: SpoolHeader = serde_json::from_str(header_line.trim())?;

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
    let mut replayed = 0usize;

    while let Some(frame) = Frame::read_from(&mut reader)? {
        if let Frame::Data { captured_at, bytes } = frame {
            replayed += bytes.len();
            // A single corrupt frame should not cost the whole run.
            let _ = daemon.ingest(&handle, &live, &bytes, captured_at);
        }
    }

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

/// Drains every spool file present at startup, oldest first.
pub fn drain_all(daemon: &Arc<Daemon>) -> usize {
    let dir = logbear_core::paths::spool_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "spool").unwrap_or(false))
        .collect();
    files.sort();

    let mut done = 0;
    for path in files {
        match ingest(daemon, &path) {
            Ok(_) => done += 1,
            Err(e) => {
                // A spool that will not parse is moved aside rather than retried forever
                // or deleted: it is the only copy of that output.
                eprintln!("logbeard: could not ingest {}: {e}", path.display());
                let _ = std::fs::rename(&path, path.with_extension("spool.bad"));
            }
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use logbear_core::model::{RunId, RunStamp};
    use std::io::Write;

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

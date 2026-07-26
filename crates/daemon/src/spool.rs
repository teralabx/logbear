//! Backfill of spool files.
//!
//! When the daemon is down, upgrading, or wedged, the shim writes captured bytes to a
//! spool file and carries on — the wrapped process must never notice (C2). This is the
//! other half of that: on next contact, the spool becomes a real run, in order, with its
//! original timestamps.

use crate::state::Daemon;
use runlog_core::proto::{Frame, SpoolHeader};
use runlog_core::Result;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

/// Ingest one spool file and delete it. Returns the number of bytes replayed.
pub fn ingest(daemon: &Arc<Daemon>, path: &Path) -> Result<usize> {
    let file = std::fs::File::open(path).map_err(|e| runlog_core::Error::io(path, e))?;
    let mut reader = BufReader::new(file);

    let mut header_line = String::new();
    reader.read_line(&mut header_line)?;
    let header: SpoolHeader = serde_json::from_str(header_line.trim())?;

    let (handle, live) = daemon.start_run(&header.stamp)?;
    let mut replayed = 0usize;

    while let Some(frame) = Frame::read_from(&mut reader)? {
        if let Frame::Data { captured_at, bytes } = frame {
            replayed += bytes.len();
            // A single corrupt frame should not cost the whole run; keep going and let
            // the rest land.
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

/// Drain every spool file present at startup, oldest first.
pub fn drain_all(daemon: &Arc<Daemon>) -> usize {
    let dir = runlog_core::paths::spool_dir();
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
                // A spool we cannot parse is moved aside rather than retried forever,
                // and rather than deleted — it is the only copy of that output.
                eprintln!("runlogd: could not ingest {}: {e}", path.display());
                let _ = std::fs::rename(&path, path.with_extension("spool.bad"));
            }
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use runlog_core::model::RunStamp;
    use std::io::Write;

    #[test]
    fn a_spooled_run_becomes_a_real_run() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("RUNLOG_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();

        let work = tmp.path().join("proj");
        std::fs::create_dir_all(&work).unwrap();
        let path = runlog_core::paths::spool_dir().join("t.spool");
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
                started_at: runlog_core::now_micros(),
                exit_code: Some(1),
                signal: None,
                extra: Default::default(),
            };
            writeln!(f, "{}", serde_json::to_string(&header).unwrap()).unwrap();
            Frame::Data {
                captured_at: runlog_core::now_micros(),
                bytes: b"Error: spooled while the daemon was down\n".to_vec(),
            }
            .write_to(&mut f)
            .unwrap();
        }

        let n = drain_all(&daemon);
        assert_eq!(n, 1);
        assert!(!path.exists(), "the spool should be gone once the run is durable");

        let handle = daemon
            .resolve(&runlog_core::proto::ProjectRef::Path(
                work.to_string_lossy().into_owned(),
            ))
            .unwrap();
        let runs = runlog_core::store::read::list_runs(&handle.db, 10, 0).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].exit_code, Some(1));
        assert_eq!(runs[0].counters.n_error, 1);
    }

    #[test]
    fn an_unreadable_spool_is_set_aside_not_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("RUNLOG_HOME", tmp.path());
        let daemon = Daemon::open().unwrap();
        let path = runlog_core::paths::spool_dir().join("broken.spool");
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

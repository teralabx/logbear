//! A secret passed on the command line must not reach the run row, whichever way the run
//! came to exist.

use logbear_core::model::RunStamp;
use logbear_core::redact::Redactor;
use logbear_core::store::{index::IndexDb, read, Db, Writer};
use std::sync::Arc;

struct Fixture {
    _tmp: tempfile::TempDir,
    db: Db,
    index: IndexDb,
    cwd: String,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let index = IndexDb::open(&tmp.path().join("index.db")).unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = tmp.path().join("p.db");
    let project = index.register(&root, "proj", &db_path).unwrap();
    let db = Db::open(&db_path, project.id).unwrap();
    Fixture {
        cwd: root.to_string_lossy().into_owned(),
        _tmp: tmp,
        db,
        index,
    }
}

fn stamp(cwd: &str) -> RunStamp {
    RunStamp {
        command: vec!["app".into(), "--password=hunter2".into(), "--token".into(), "hunter3".into()],
        cwd: cwd.to_string(),
        ..Default::default()
    }
}

fn redactor() -> Arc<Redactor> {
    Arc::new(Redactor::new(true, &[]))
}

#[test]
fn a_started_run_stores_no_argv_secret() {
    let f = fixture();
    let w = Writer::start(&f.db, &f.index, &stamp(&f.cwd), redactor()).unwrap();

    let run = read::get_run(&f.db, w.run_id()).unwrap();
    let line = run.command_line();
    assert!(!line.contains("hunter2") && !line.contains("hunter3"), "{line}");
    assert!(line.contains("--password="), "the flag should stay readable: {line}");

    // The index keeps its own copy for the cross-project views.
    let index = rusqlite::Connection::open(f._tmp.path().join("index.db")).unwrap();
    let summary: String = index
        .query_row("SELECT command FROM run_summaries LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert!(!summary.contains("hunter2"), "index summary leaked: {summary}");
}

#[test]
fn a_masked_run_can_still_be_resumed() {
    let f = fixture();
    let s = stamp(&f.cwd);
    let at = logbear_core::now_micros();
    let run_id = Writer::start(&f.db, &f.index, &s, redactor()).unwrap().run_id();

    // The shim's stamp still holds the real argv; only the stored copy is masked.
    let resumed = Writer::resume(&f.db, run_id, &s, at, redactor()).unwrap();
    assert!(resumed.is_some(), "a masked command must still match its own run");
}

#[test]
fn a_disabled_redactor_keeps_the_command_as_typed() {
    let f = fixture();
    let w = Writer::start(&f.db, &f.index, &stamp(&f.cwd), Arc::new(Redactor::disabled())).unwrap();
    let run = read::get_run(&f.db, w.run_id()).unwrap();
    assert!(run.command_line().contains("hunter2"));
}

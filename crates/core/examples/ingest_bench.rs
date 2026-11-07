//! A repeatable ingest benchmark: MB/s and disk per MB in, for the two workloads that
//! bound the store.
//!
//! ```text
//! cargo run --release -p logbear-core --example ingest_bench -- [--mb 100] [--split]
//! ```
//!
//! It drives [`Writer`] the way the daemon does: 64 KB frames (the size the shim reads),
//! a tick on the maintenance interval, and a finish at the end. Everything is written to a
//! fresh temporary directory, never to `LOGBEAR_HOME`.
//!
//! * `low-cardinality` has a few templates and no ids a normaliser would miss, the typical
//!   dev server.
//! * `unique-id` puts bare hex and base64 request ids on every line, the worst case.
//!
//! The figures bypass the socket, so they are an upper bound on what a real run sees and
//! are meant for before/after comparison on one machine. `--split` adds where the disk
//! went, by table, from `dbstat`.

use logbear_core::model::RunStamp;
use logbear_core::now_micros;
use logbear_core::redact::Redactor;
use logbear_core::store::{index::IndexDb, Db, Writer};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FRAME: usize = 64 * 1024;
const TICK: Duration = Duration::from_millis(200);

/// A small xorshift, so a run is reproducible and the generator costs nothing.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn low_cardinality(i: u64, rng: &mut Rng, out: &mut Vec<u8>) {
    use std::io::Write;
    let ms = rng.next() % 90;
    let line = match i % 6 {
        0 => format!("12:03:04.{:03} [info] GET /api/users/{} 200 in {ms}ms\n", i % 1000, i % 977),
        1 => format!("12:03:04.{:03} [info] POST /api/sessions 201 in {ms}ms\n", i % 1000),
        2 => format!("12:03:04.{:03} [debug] QUERY OK source=\"users\" db={ms}ms\n", i % 1000),
        3 => format!("12:03:04.{:03} [info] Sent 200 in {ms}ms\n", i % 1000),
        4 => format!("12:03:04.{:03} [debug] Processing with UserController.show/2\n", i % 1000),
        _ => format!("12:03:04.{:03} [warn] pool at {}% capacity\n", i % 1000, ms % 100),
    };
    out.write_all(line.as_bytes()).unwrap();
}

fn unique_id(i: u64, rng: &mut Rng, out: &mut Vec<u8>) {
    use std::io::Write;
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut b64 = [0u8; 20];
    for b in &mut b64 {
        *b = B64[(rng.next() % 64) as usize];
    }
    let line = format!(
        "12:03:04.{:03} [info] request_id={} req={:016x}{:08x} GET /api/users 200 in {}ms\n",
        i % 1000,
        std::str::from_utf8(&b64).unwrap(),
        rng.next(),
        rng.next() as u32,
        rng.next() % 90,
    );
    out.write_all(line.as_bytes()).unwrap();
}

fn frames(mb: usize, line: fn(u64, &mut Rng, &mut Vec<u8>)) -> (Vec<Vec<u8>>, u64) {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut frames = Vec::new();
    let mut current = Vec::with_capacity(FRAME + 256);
    let mut total = 0usize;
    let mut i = 0u64;
    while total < mb * 1024 * 1024 {
        line(i, &mut rng, &mut current);
        i += 1;
        if current.len() >= FRAME {
            total += current.len();
            frames.push(std::mem::replace(&mut current, Vec::with_capacity(FRAME + 256)));
        }
    }
    (frames, i)
}

/// Bytes per object, largest first, FTS shadow tables rolled into one row.
fn split(db: &Db) -> Vec<(String, i64)> {
    let rows: Vec<(String, i64)> = db
        .with(|c| {
            let mut stmt = c.prepare("SELECT name, sum(pgsize) FROM dbstat GROUP BY name")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap();
    let mut merged: std::collections::BTreeMap<String, i64> = Default::default();
    for (name, bytes) in rows {
        let name = if name.starts_with("entries_fts") { "entries_fts".to_string() } else { name };
        *merged.entry(name).or_default() += bytes;
    }
    let mut out: Vec<_> = merged.into_iter().collect();
    out.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
    out
}

fn run(name: &str, mb: usize, show_split: bool, line: fn(u64, &mut Rng, &mut Vec<u8>)) {
    let (frames, lines) = frames(mb, line);
    let bytes_in: usize = frames.iter().map(|f| f.len()).sum();

    let tmp = tempfile::tempdir().expect("tempdir");
    let index = IndexDb::open(&tmp.path().join("index.db")).expect("index");
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).expect("root");
    let db_path = tmp.path().join("p.db");
    let project = index.register(&root, "proj", &db_path).expect("register");
    let db = Db::open(&db_path, project.id).expect("db");

    let stamp = RunStamp {
        command: vec!["bench".into()],
        cwd: root.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let mut writer =
        Writer::start(&db, &index, &stamp, Arc::new(Redactor::new(true, &[]))).expect("start");

    let started = Instant::now();
    let mut last_tick = started;
    let mut peak_wal = 0u64;
    let wal = tmp.path().join("p.db-wal");
    for frame in &frames {
        writer.push(frame, now_micros()).expect("push");
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            writer.tick(now_micros()).expect("tick");
            peak_wal = peak_wal.max(std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0));
        }
    }
    writer.finish(&index, Some(0), None).expect("finish");
    let elapsed = started.elapsed().as_secs_f64();
    drop(writer);

    // Disk is measured once everything has settled, with the WAL folded in, so the ratio
    // does not depend on where the last checkpoint happened to land.
    db.with(|c| {
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    })
    .expect("checkpoint");
    let disk = db.size_bytes();
    let mb_in = bytes_in as f64 / (1024.0 * 1024.0);

    println!(
        "{name:<16} {mb_in:>6.0} MB in  {lines:>9} lines  {:>7.1} MB/s  {:>9.0} lines/s  \
         disk {:>5.2}x  peak WAL {:>6.1} MB",
        mb_in / elapsed,
        lines as f64 / elapsed,
        disk as f64 / bytes_in as f64,
        peak_wal as f64 / (1024.0 * 1024.0),
    );
    if show_split {
        for (name, bytes) in split(&db) {
            println!(
                "    {name:<24} {:>8.1} MB  {:>5.2}x",
                bytes as f64 / (1024.0 * 1024.0),
                bytes as f64 / bytes_in as f64
            );
        }
    }
}

fn main() {
    let mut mb = 100usize;
    let mut show_split = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mb" => mb = args.next().and_then(|v| v.parse().ok()).expect("--mb <n>"),
            "--split" => show_split = true,
            other => {
                eprintln!("usage: ingest_bench [--mb <n>] [--split] (got {other})");
                std::process::exit(2);
            }
        }
    }
    run("low-cardinality", mb, show_split, low_cardinality);
    run("unique-id", mb, show_split, unique_id);
}

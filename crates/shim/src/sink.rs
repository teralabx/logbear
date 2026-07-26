//! The daemon channel, and the spool it degrades to.
//!
//! This module exists to satisfy C2: **the shim can never degrade the user's process.**
//! The read→stdout path in [`crate::wrap`] hands bytes to [`Sink::offer`], which is a
//! `try_send` into a bounded queue and nothing else. It cannot block, cannot fail upward,
//! and cannot make the wrapped command slower, whatever state the daemon is in.
//!
//! Everything that can block — socket writes, file creation, waiting for a reply — happens
//! on the sender thread behind that queue. The thread has exactly one failure response:
//! stop talking to the daemon and start writing a spool file. It never retries into a wall
//! and never reports a problem to the user, because a dev server that keeps running while
//! runlog quietly loses a minute of history is the correct trade, and the reverse is not.
//!
//! **Why the queue can drop.** If the daemon accepts a connection and then wedges, the
//! socket buffer fills and the sender thread blocks inside a write. The queue fills behind
//! it. At that point the reader either blocks — forbidden — or drops. It drops, counts
//! what it dropped, and flips a flag that makes the sender abandon the socket for the
//! spool as soon as it comes back. The loss is bounded by the queue depth and recorded in
//! the spool header, so it is visible rather than silent.

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use runlog_core::client::Client;
use runlog_core::model::{RunId, RunStamp};
use runlog_core::paths;
use runlog_core::proto::{Frame, Request, SpoolHeader};
use std::collections::BTreeMap;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Deep enough to ride out a scheduler hiccup or a batch commit, shallow enough that a
/// wedged daemon costs a bounded amount of memory rather than an unbounded one.
const QUEUE_DEPTH: usize = 1024;

/// How long the sender waits for more bytes before flushing what it has. Capture is
/// batched so a chatty stream costs one syscall per batch, but a quiet stream must not
/// leave the last line sitting in a buffer where the live view cannot see it.
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// Frames queued before an unconditional flush, so a firehose does not grow the buffer.
const FLUSH_EVERY: usize = 64;

/// How long the exit path waits for the sender to finish. Bounded: the child has already
/// exited and the user is waiting for their prompt back.
const FINISH_GRACE: Duration = Duration::from_millis(2000);

pub const SPOOL_VERSION: u32 = 1;

enum Msg {
    Data { captured_at: i64, bytes: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Finish { exit_code: Option<i32>, signal: Option<String> },
}

pub struct Sink {
    tx: Sender<Msg>,
    dropped: Arc<AtomicU64>,
    degrade: Arc<AtomicBool>,
    done: Receiver<()>,
}

impl Sink {
    /// `client` is `None` when the daemon could not be reached at all, in which case the
    /// sink spools from the first byte.
    pub fn start(
        client: Option<Client>,
        run_id: Option<RunId>,
        token: String,
        stamp: RunStamp,
        started_at: i64,
    ) -> Sink {
        let (tx, rx) = bounded(QUEUE_DEPTH);
        let (done_tx, done) = bounded(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let degrade = Arc::new(AtomicBool::new(false));

        let state = SenderState {
            client: client.zip(run_id).map(|(c, id)| (c, id)),
            spool: None,
            token,
            stamp,
            started_at,
            dropped: dropped.clone(),
            degrade: degrade.clone(),
            pending: 0,
        };

        std::thread::Builder::new()
            .name("runlog-sink".into())
            .spawn(move || {
                state.run(rx);
                let _ = done_tx.send(());
            })
            .ok();

        Sink {
            tx,
            dropped,
            degrade,
            done,
        }
    }

    /// A cheap clone for the signal-watching thread, which has to report resizes but must
    /// not own the sink or be able to finish it.
    pub fn handle(&self) -> SinkHandle {
        SinkHandle {
            tx: self.tx.clone(),
        }
    }

    /// Never blocks. This is the whole point of the module.
    pub fn offer(&self, captured_at: i64, bytes: &[u8]) {
        let msg = Msg::Data {
            captured_at,
            bytes: bytes.to_vec(),
        };
        if let Err(TrySendError::Full(_)) = self.tx.try_send(msg) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            // The sender is stuck on a socket that is not draining. Tell it to give up on
            // the daemon; the spool is a local file and cannot wedge the same way.
            self.degrade.store(true, Ordering::Relaxed);
        }
    }

    /// Report the outcome and wait, briefly, for the sender to land it. Both halves are
    /// time-boxed: the child has already exited and the user is owed their prompt back,
    /// so a wedged daemon costs them the grace period and not a second more.
    pub fn finish(self, exit_code: Option<i32>, signal: Option<String>) {
        let msg = Msg::Finish { exit_code, signal };
        if self.tx.send_timeout(msg, FINISH_GRACE).is_err() {
            self.degrade.store(true, Ordering::Relaxed);
        }
        drop(self.tx);
        let _ = self.done.recv_timeout(FINISH_GRACE);
    }
}

#[derive(Clone)]
pub struct SinkHandle {
    tx: Sender<Msg>,
}

impl SinkHandle {
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.try_send(Msg::Resize { cols, rows });
    }
}

// ---------------------------------------------------------------------------
// Sender thread
// ---------------------------------------------------------------------------

struct SenderState {
    client: Option<(Client, RunId)>,
    spool: Option<SpoolWriter>,
    token: String,
    stamp: RunStamp,
    started_at: i64,
    dropped: Arc<AtomicU64>,
    degrade: Arc<AtomicBool>,
    pending: usize,
}

impl SenderState {
    fn run(mut self, rx: Receiver<Msg>) {
        // No daemon at all: spool from the first byte rather than discovering it later.
        if self.client.is_none() {
            self.fall_back();
        }
        loop {
            if self.degrade.load(Ordering::Relaxed) {
                self.fall_back();
            }
            match rx.recv_timeout(FLUSH_INTERVAL) {
                Ok(Msg::Data { captured_at, bytes }) => {
                    self.write_data(captured_at, &bytes);
                }
                Ok(Msg::Resize { cols, rows }) => self.write_resize(cols, rows),
                Ok(Msg::Finish { exit_code, signal }) => {
                    self.write_finish(exit_code, signal);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => self.flush(),
                // The wrap path exited without a Finish — a panic, or a hard kill. Land
                // what we have rather than dropping the tail.
                Err(RecvTimeoutError::Disconnected) => {
                    self.write_finish(None, None);
                    return;
                }
            }
        }
    }

    fn write_data(&mut self, captured_at: i64, bytes: &[u8]) {
        let frame = Frame::Data {
            captured_at,
            bytes: bytes.to_vec(),
        };
        if let Some((client, _)) = self.client.as_mut() {
            match client.queue(&frame) {
                Ok(()) => {
                    self.pending += 1;
                    if self.pending >= FLUSH_EVERY {
                        self.flush();
                    }
                    return;
                }
                Err(_) => self.fall_back(),
            }
        }
        if let Some(spool) = self.spool.as_mut() {
            let _ = spool.write_frame(&frame);
        }
    }

    fn write_resize(&mut self, cols: u16, rows: u16) {
        let Some((client, run_id)) = self.client.as_mut() else {
            return;
        };
        let req = Request::Resize {
            run_id: *run_id,
            cols,
            rows,
        };
        let ok = Frame::json(&req)
            .ok()
            .map(|f| client.queue(&f).is_ok())
            .unwrap_or(false);
        if !ok {
            self.fall_back();
        }
    }

    fn write_finish(&mut self, exit_code: Option<i32>, signal: Option<String>) {
        if let Some((client, run_id)) = self.client.as_mut() {
            let req = Request::FinishRun {
                run_id: *run_id,
                exit_code,
                signal: signal.clone(),
            };
            let sent = Frame::json(&req)
                .ok()
                .map(|f| client.queue(&f).is_ok() && client.flush().is_ok())
                .unwrap_or(false);
            if sent {
                return;
            }
            self.fall_back();
        }
        if let Some(spool) = self.spool.as_mut() {
            let _ = spool.finish(exit_code, signal, self.dropped.load(Ordering::Relaxed));
        }
    }

    fn flush(&mut self) {
        self.pending = 0;
        if let Some((client, _)) = self.client.as_mut() {
            if client.flush().is_err() {
                self.fall_back();
            }
        }
    }

    /// Abandon the daemon for the rest of this run. Deliberately one-way: a socket that
    /// has failed once under load will fail again, and flapping between the two would
    /// interleave the run's bytes across two sinks.
    fn fall_back(&mut self) {
        let continues = self.client.take().map(|(_, id)| id);
        self.degrade.store(false, Ordering::Relaxed);
        if self.spool.is_some() {
            return;
        }
        let mut extra = BTreeMap::new();
        if let Some(id) = continues {
            // The daemon accepted a StartRun and then stopped listening; this file is the
            // rest of that run, not a new one.
            extra.insert("continues_run".to_string(), id.to_string());
        }
        let header = SpoolHeader {
            version: SPOOL_VERSION,
            stamp: self.stamp.clone(),
            started_at: self.started_at,
            exit_code: None,
            signal: None,
            extra,
        };
        self.spool = SpoolWriter::create(&paths::spool_file(&self.token), header).ok();
    }
}

// ---------------------------------------------------------------------------
// Spool files
// ---------------------------------------------------------------------------

/// `~/.runlog/spool/<token>.spool`
///
/// ```text
/// <SpoolHeader as JSON><padding spaces>\n
/// [frame][frame][frame]...
/// ```
///
/// The header has to be the first line, but two of its fields — exit code and signal —
/// are only known once the run is over. Rather than rewriting the file or buffering it in
/// memory, the header line is written padded to a reserved length and rewritten in place
/// at the end. The file length never changes, the frames never move, and a reader that
/// does `read_line` then `serde_json::from_str` is unaffected by the padding, because JSON
/// permits trailing whitespace.
pub struct SpoolWriter {
    file: BufWriter<std::fs::File>,
    header: SpoolHeader,
    reserve: usize,
}

impl SpoolWriter {
    pub fn create(path: &Path, header: SpoolHeader) -> std::io::Result<SpoolWriter> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        // Advisory lock held for the writer's lifetime, so a concurrently starting shim
        // does not hand a half-written spool to the daemon.
        lock_exclusive(&file);

        let encoded = serde_json::to_vec(&header)?;
        // Slack for the exit code, the signal name and a drop count: everything that gets
        // filled in later is small and bounded.
        let reserve = encoded.len() + 512;
        let mut file = BufWriter::new(file);
        write_padded_header(&mut file, &encoded, reserve)?;
        Ok(SpoolWriter {
            file,
            header,
            reserve,
        })
    }

    pub fn write_frame(&mut self, frame: &Frame) -> std::io::Result<()> {
        frame.write_to(&mut self.file)
    }

    pub fn finish(
        &mut self,
        exit_code: Option<i32>,
        signal: Option<String>,
        dropped: u64,
    ) -> std::io::Result<()> {
        self.header.exit_code = exit_code;
        self.header.signal = signal;
        if dropped > 0 {
            self.header
                .extra
                .insert("dropped_chunks".into(), dropped.to_string());
        }
        self.file.flush()?;
        let encoded = serde_json::to_vec(&self.header)?;
        // If the finished header somehow outgrew its reserve the started-state header
        // already on disk is still valid; a missing exit code is better than a corrupt
        // file, so leave it alone.
        if encoded.len() + 1 <= self.reserve {
            let file = self.file.get_mut();
            file.seek(SeekFrom::Start(0))?;
            write_padded_header(file, &encoded, self.reserve)?;
            file.seek(SeekFrom::End(0))?;
        }
        self.file.flush()
    }
}

fn write_padded_header<W: Write>(w: &mut W, encoded: &[u8], reserve: usize) -> std::io::Result<()> {
    w.write_all(encoded)?;
    let pad = reserve.saturating_sub(encoded.len() + 1);
    for _ in 0..pad {
        w.write_all(b" ")?;
    }
    w.write_all(b"\n")
}

fn lock_exclusive(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Hand every spool file the daemon has not seen back to it.
///
/// Run at startup on its own connection and its own thread, before the child is spawned,
/// so a backlog from a weekend of daemon downtime drains without the user ever noticing
/// and without delaying the command they just typed. Files locked by a live shim are
/// skipped — they are still being written.
pub fn drain(skip_token: &str) {
    let dir = paths::spool_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "spool"))
        .filter(|p| {
            p.file_stem()
                .map(|s| s != std::ffi::OsStr::new(skip_token))
                .unwrap_or(false)
        })
        .collect();
    if paths.is_empty() {
        return;
    }
    paths.sort();

    let Ok(mut client) = Client::connect() else {
        return;
    };
    for path in paths {
        if !is_unlocked(&path) {
            continue;
        }
        let req = Request::IngestSpool {
            path: path.to_string_lossy().into_owned(),
        };
        match client.call::<serde_json::Value>(&req) {
            // Removing ours rather than trusting the daemon to: a spool file that is
            // ingested and left behind would be re-sent on every subsequent run.
            Ok(_) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => return,
        }
    }
}

fn is_unlocked(path: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let fd = file.as_raw_fd();
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return false;
    }
    unsafe { libc::flock(fd, libc::LOCK_UN) };
    true
}

/// A run identifier that does not need the daemon: 128 bits of time plus process plus
/// address-space entropy, which is enough for a filename that only has to be unique among
/// the spool files on one machine.
pub fn local_token() -> String {
    let now = runlog_core::now_micros() as u128;
    let pid = std::process::id() as u128;
    let addr = &now as *const u128 as usize as u128;
    format!("{:016x}{:08x}{:08x}", now, pid as u32, addr as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use runlog_core::proto::Frame;
    use std::io::{BufRead, BufReader, Read};

    fn sample_header() -> SpoolHeader {
        SpoolHeader {
            version: SPOOL_VERSION,
            stamp: RunStamp {
                command: vec!["mix".into(), "phx.server".into()],
                cwd: "/tmp/app".into(),
                ..Default::default()
            },
            started_at: 1_700_000_000_000_000,
            exit_code: None,
            signal: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn spool_round_trips_header_and_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.spool");

        {
            let mut w = SpoolWriter::create(&path, sample_header()).unwrap();
            w.write_frame(&Frame::Data {
                captured_at: 11,
                bytes: b"hello\r\n".to_vec(),
            })
            .unwrap();
            w.write_frame(&Frame::Data {
                captured_at: 22,
                bytes: b"world\r\n".to_vec(),
            })
            .unwrap();
            w.finish(Some(3), None, 0).unwrap();
        }

        let mut reader = BufReader::new(std::fs::File::open(&path).unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let header: SpoolHeader = serde_json::from_str(&line).unwrap();
        assert_eq!(header.version, SPOOL_VERSION);
        assert_eq!(header.stamp.command, vec!["mix", "phx.server"]);
        assert_eq!(header.exit_code, Some(3), "exit code was not written back");

        let mut got = Vec::new();
        while let Some(frame) = Frame::read_from(&mut reader).unwrap() {
            match frame {
                Frame::Data { captured_at, bytes } => got.push((captured_at, bytes)),
                Frame::Json(_) => panic!("spool bodies hold data frames only"),
            }
        }
        assert_eq!(
            got,
            vec![(11, b"hello\r\n".to_vec()), (22, b"world\r\n".to_vec())]
        );
    }

    #[test]
    fn rewriting_the_header_does_not_move_the_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("len.spool");

        let mut w = SpoolWriter::create(&path, sample_header()).unwrap();
        w.write_frame(&Frame::Data {
            captured_at: 1,
            bytes: vec![b'x'; 100],
        })
        .unwrap();
        w.file.flush().unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        w.finish(Some(0), Some("SIGTERM".into()), 7).unwrap();
        drop(w);
        assert_eq!(before, std::fs::metadata(&path).unwrap().len());

        let mut reader = BufReader::new(std::fs::File::open(&path).unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let header: SpoolHeader = serde_json::from_str(&line).unwrap();
        assert_eq!(header.signal.as_deref(), Some("SIGTERM"));
        assert_eq!(header.extra.get("dropped_chunks").map(String::as_str), Some("7"));

        let frame = Frame::read_from(&mut reader).unwrap().unwrap();
        match frame {
            Frame::Data { bytes, .. } => assert_eq!(bytes.len(), 100),
            _ => panic!("frame boundary moved"),
        }
    }

    #[test]
    fn a_header_line_is_padded_but_still_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pad.spool");
        let w = SpoolWriter::create(&path, sample_header()).unwrap();
        drop(w);

        let mut raw = Vec::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_end(&mut raw)
            .unwrap();
        assert_eq!(raw.iter().filter(|b| **b == b'\n').count(), 1);
    }

    #[test]
    fn local_tokens_are_distinct() {
        let a = local_token();
        let b = local_token();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_sink_with_no_daemon_spools_and_never_blocks() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("RUNLOG_HOME", dir.path());

        let token = local_token();
        let sink = Sink::start(None, None, token.clone(), RunStamp::default(), 1);
        for i in 0..(QUEUE_DEPTH * 2) {
            sink.offer(i as i64, b"line\r\n");
        }
        sink.finish(Some(0), None);

        let path = paths::spool_file(&token);
        assert!(path.exists(), "no spool file at {}", path.display());
        std::env::remove_var("RUNLOG_HOME");
    }
}

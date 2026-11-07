//! The daemon channel, and the spool files behind it.
//!
//! [`Sink::offer`] is a non-blocking send and nothing else, so the wrapped command is never
//! slowed by the daemon's state. Everything else happens on the sender thread, which has
//! one rule: it never blocks on the daemon either. Contact with the daemon is made on a
//! thread of its own while the child is already running, and the socket is non-blocking.
//!
//! Output the socket cannot take goes to a spool file instead, and is not lost. Frames wait
//! in [`Sender::out`] up to [`OUT_BYTES`]; past that they go to a *segment*, a spool file on
//! local disk. When the socket catches up, an `IngestSegment` marker goes down the same
//! stream, so the daemon replays the segment exactly where those bytes belong and everything
//! after it arrives in order. A burst therefore costs disk rather than the run, and the
//! stream carries on live afterwards instead of being abandoned.
//!
//! Losing the daemon works the same way, with the marker waiting for a reconnect. If the
//! command ends first, what remains stays on disk with the exit code in its header, and the
//! daemon's own spool sweep picks it up.
//!
//! The memory budget is in bytes rather than messages: a line-buffered child writes one line
//! per read, and a message cap is overrun by any ordinary burst.

use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender as ChannelTx};
use logbear_core::client::{spawn_daemon, Client};
use logbear_core::model::{RunId, RunStamp};
use logbear_core::paths;
use logbear_core::proto::{Frame, Request, SpoolHeader, PROTOCOL, SPOOL_VERSION};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufWriter, IoSlice, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Captured bytes waiting for the sender thread. It only stops draining this if the disk
/// itself stalls, so reaching it means dropping is the only option left.
const QUEUE_BYTES: usize = 32 * 1024 * 1024;

/// Encoded frames held in memory for the socket. Beyond this they go to a segment.
const OUT_BYTES: usize = 4 * 1024 * 1024;

/// Size at which a segment is closed and the next one started.
const SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

/// Disk one run may hold in spool files. A firehose the daemon cannot keep up with loses its
/// oldest output, never its newest: the error that ended the run is the part that matters.
const SPOOL_BUDGET: u64 = 512 * 1024 * 1024;

/// How long the exit path waits for the socket: the child has already exited and the user
/// is waiting for their prompt back. What does not fit stays in a spool file.
const FINISH_GRACE: Duration = Duration::from_millis(1000);

/// How long a finished command waits for contact with the daemon before spooling instead.
const HANDSHAKE_GRACE: Duration = Duration::from_millis(400);

/// Upper bound on any reply the handshake waits for.
const REPLY_TIMEOUT: Duration = Duration::from_millis(1000);

/// Time between attempts to reach a daemon that has gone away.
const RECONNECT_EVERY: Duration = Duration::from_secs(1);

/// One attempt in this many also tries to start the daemon.
const START_DAEMON_EVERY: u32 = 5;

/// How often an out-of-date daemon may be asked to make way for a new one.
const REPLACE_AT_MOST_EVERY: Duration = Duration::from_secs(3600);

/// Slices handed to one `writev`.
const WRITE_BATCH: usize = 64;

enum Msg {
    Data { captured_at: i64, bytes: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Finish { exit_code: Option<i32>, signal: Option<String> },
}

/// What the sink needs to know about the command it is recording.
pub struct RunInfo {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub started_at: i64,
    pub token: String,
}

pub struct Sink {
    tx: ChannelTx<Msg>,
    /// Payload bytes sitting in the queue, charged by `offer` and refunded by the sender.
    queued: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    done: Receiver<()>,
}

impl Sink {
    pub fn start(info: RunInfo) -> Sink {
        let (tx, rx) = unbounded();
        let (done_tx, done) = bounded(1);
        let queued = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicU64::new(0));

        let stamp = logbear_core::project::quick_stamp(
            info.command.clone(),
            &info.cwd,
            info.cols,
            info.rows,
        );
        let sender = Sender {
            link: Link::Connecting(connect_first(info.command, info.cwd, info.cols, info.rows)),
            out: VecDeque::new(),
            out_bytes: 0,
            head_sent: 0,
            segment: None,
            closed: VecDeque::new(),
            next_segment: 1,
            token: info.token,
            stamp,
            started_at: info.started_at,
            run_id: None,
            queued: queued.clone(),
            dropped: dropped.clone(),
            attempts: 0,
        };

        std::thread::Builder::new()
            .name("logbear-sink".into())
            .spawn(move || {
                sender.run(rx);
                let _ = done_tx.send(());
            })
            .ok();

        Sink { tx, queued, dropped, done }
    }

    /// A clone for the signal-watching thread, which reports resizes but must not own the
    /// sink or be able to finish it.
    pub fn handle(&self) -> SinkHandle {
        SinkHandle { tx: self.tx.clone() }
    }

    /// Queues a chunk of captured output. Never blocks.
    pub fn offer(&self, captured_at: i64, bytes: &[u8]) {
        if self.queued.fetch_add(bytes.len(), Ordering::Relaxed) + bytes.len() > QUEUE_BYTES {
            self.queued.fetch_sub(bytes.len(), Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let msg = Msg::Data { captured_at, bytes: bytes.to_vec() };
        if self.tx.send(msg).is_err() {
            // The sender has gone; nothing will refund this charge.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Reports the outcome and waits for the sender to land it, on the socket or on disk.
    ///
    /// The wait is bounded, so a wedged daemon costs the user a fixed moment and no more.
    pub fn finish(self, exit_code: Option<i32>, signal: Option<String>) {
        let _ = self.tx.send(Msg::Finish { exit_code, signal });
        drop(self.tx);
        let _ = self.done.recv_timeout(HANDSHAKE_GRACE + FINISH_GRACE + Duration::from_secs(1));
    }
}

#[derive(Clone)]
pub struct SinkHandle {
    tx: ChannelTx<Msg>,
}

impl SinkHandle {
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.try_send(Msg::Resize { cols, rows });
    }
}

// ---------------------------------------------------------------------------
// Reaching the daemon
// ---------------------------------------------------------------------------

/// The result of an attempt to reach the daemon.
struct Contact {
    /// The full stamp, when this attempt had to work it out.
    stamp: Option<RunStamp>,
    link: Option<(UnixStream, RunId)>,
}

/// First contact, on its own thread: the stamp shells out to git and the daemon may be
/// wedged, and neither may hold up a command that is already running.
fn connect_first(command: Vec<String>, cwd: PathBuf, cols: u16, rows: u16) -> Receiver<Contact> {
    attempt(move || {
        let stamp = logbear_core::project::stamp(command, &cwd, cols, rows);
        let started = Client::connect_or_start().ok();
        let link = started.and_then(|client| open_run(client, &stamp, None));
        Contact { stamp: Some(stamp), link }
    })
}

/// A later attempt, once contact has been lost or could not be made.
fn reconnect(
    stamp: RunStamp,
    resume: Option<(RunId, i64)>,
    start_daemon: bool,
) -> Receiver<Contact> {
    attempt(move || {
        let client = match Client::connect() {
            Ok(client) => Some(client),
            Err(_) if start_daemon => {
                let _ = spawn_daemon();
                None
            }
            Err(_) => None,
        };
        let link = client.and_then(|client| open_run(client, &stamp, resume));
        Contact { stamp: None, link }
    })
}

fn attempt(work: impl FnOnce() -> Contact + Send + 'static) -> Receiver<Contact> {
    let (tx, rx) = bounded(1);
    let spawned = std::thread::Builder::new()
        .name("logbear-connect".into())
        .spawn(move || {
            let _ = tx.send(work());
        });
    if spawned.is_err() {
        // The sender treats a dropped channel as a failed attempt.
    }
    rx
}

/// Agrees a protocol and opens the run, returning a socket ready for non-blocking use.
fn open_run(
    mut client: Client,
    stamp: &RunStamp,
    resume: Option<(RunId, i64)>,
) -> Option<(UnixStream, RunId)> {
    client.set_timeout(Some(REPLY_TIMEOUT)).ok()?;
    let supported = client
        .call_value(&Request::Hello { protocol: PROTOCOL })
        .ok()
        .and_then(|v| v.get("protocol").and_then(|p| p.as_u64()))
        .unwrap_or(0);
    if supported < PROTOCOL as u64 {
        // A daemon left over from before an upgrade. Ask it to go; the next attempt starts
        // the new one, and this run spools in the meantime.
        if may_replace_daemon(&paths::home().join("daemon-replaced"), REPLACE_AT_MOST_EVERY) {
            let _ = client.call_value(&Request::Shutdown);
        }
        return None;
    }
    let request = match resume {
        Some((run_id, started_at)) => {
            Request::ResumeRun { stamp: stamp.clone(), started_at, run_id }
        }
        None => Request::StartRun { stamp: stamp.clone() },
    };
    let value = client.call_value(&request).ok()?;
    // Accepts a bare id or `{"run_id": n}`, so a daemon that changes its response shape
    // does not silently turn every run into a spool file.
    let run_id = value
        .as_i64()
        .or_else(|| value.get("run_id").and_then(|v| v.as_i64()))
        .or_else(|| value.get("id").and_then(|v| v.as_i64()))?;
    let stream = client.into_stream();
    stream.set_nonblocking(true).ok()?;
    stream.set_read_timeout(None).ok()?;
    Some((stream, run_id))
}

/// Limits replacing an old daemon. If a service manager keeps restarting the same old
/// binary, asking it to leave on every command would only cut off everyone else's runs.
fn may_replace_daemon(marker: &Path, every: Duration) -> bool {
    let recent = std::fs::metadata(marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < every);
    if recent {
        return false;
    }
    std::fs::write(marker, b"").is_ok()
}

// ---------------------------------------------------------------------------
// Sender thread
// ---------------------------------------------------------------------------

enum Link {
    /// An attempt is in flight.
    Connecting(Receiver<Contact>),
    Up(UnixStream),
    Down { retry_at: Instant },
}

/// One unit queued for the socket.
struct Item {
    bytes: Vec<u8>,
    kind: Kind,
}

#[derive(PartialEq)]
enum Kind {
    /// Captured output, which must survive losing the socket.
    Data,
    /// Refers to the socket's own state, and is meaningless without it.
    Control,
}

/// A finished segment. The handle keeps its lock, so the daemon's sweep leaves the file to
/// the in-band marker for as long as this process is alive.
struct ClosedSegment {
    path: PathBuf,
    bytes: u64,
    _lock: std::fs::File,
}

struct Sender {
    link: Link,
    out: VecDeque<Item>,
    out_bytes: usize,
    /// Bytes of the front item already written to the socket.
    head_sent: usize,
    segment: Option<SpoolWriter>,
    closed: VecDeque<ClosedSegment>,
    next_segment: u32,
    token: String,
    stamp: RunStamp,
    started_at: i64,
    run_id: Option<RunId>,
    queued: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    attempts: u32,
}

impl Sender {
    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            self.poll_link();
            self.write_socket();
            self.settle_segment();

            let wait = if self.socket_is_behind() {
                Duration::from_millis(2)
            } else {
                Duration::from_millis(50)
            };
            match rx.recv_timeout(wait) {
                Ok(msg) => {
                    let mut next = Some(msg);
                    // Everything already queued goes out in one write.
                    for _ in 0..WRITE_BATCH {
                        match next.take() {
                            Some(Msg::Finish { exit_code, signal }) => {
                                return self.finish(exit_code, signal);
                            }
                            Some(msg) => self.accept(msg),
                            None => break,
                        }
                        next = rx.try_recv().ok();
                    }
                    if let Some(Msg::Finish { exit_code, signal }) = next {
                        return self.finish(exit_code, signal);
                    } else if let Some(msg) = next {
                        self.accept(msg);
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.flush_segment(),
                // The wrap path exited without a Finish, by panic or hard kill. Land what
                // is there rather than dropping the tail.
                Err(RecvTimeoutError::Disconnected) => return self.finish(None, None),
            }
        }
    }

    fn socket_is_behind(&self) -> bool {
        matches!(self.link, Link::Up(_)) && !self.out.is_empty()
    }

    fn accept(&mut self, msg: Msg) {
        match msg {
            Msg::Data { captured_at, bytes } => {
                self.queued.fetch_sub(bytes.len(), Ordering::Relaxed);
                self.store(Frame::Data { captured_at, bytes });
            }
            Msg::Resize { cols, rows } => {
                let Some(run_id) = self.run_id else { return };
                if matches!(self.link, Link::Up(_)) {
                    self.send_control(&Request::Resize { run_id, cols, rows });
                }
            }
            Msg::Finish { .. } => {}
        }
    }

    /// Queues a frame for the socket if it may overtake nothing and fits; otherwise spools it.
    fn store(&mut self, frame: Frame) {
        let down = matches!(self.link, Link::Down { .. });
        if self.segment.is_none() && !down && self.out_bytes < OUT_BYTES {
            let mut bytes = Vec::new();
            if frame.write_to(&mut bytes).is_ok() {
                self.out_bytes += bytes.len();
                self.out.push_back(Item { bytes, kind: Kind::Data });
            }
            return;
        }
        self.spool(&frame);
    }

    fn spool(&mut self, frame: &Frame) {
        if self.segment.is_none() && !self.open_segment() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(segment) = self.segment.as_mut() else { return };
        if segment.write_frame(frame).is_err() {
            // Most likely a full disk. Freeing our own oldest output may be all that helps.
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.evict_oldest();
            return;
        }
        if segment.bytes_written >= SEGMENT_BYTES {
            self.close_segment();
            self.enforce_budget();
        }
    }

    fn open_segment(&mut self) -> bool {
        let path = paths::spool_dir().join(format!("{}-{:04}.spool", self.token, self.next_segment));
        self.next_segment += 1;
        // A shim may get here before any daemon has made the home, and it must end up
        // private whoever makes it first.
        let _ = paths::ensure_home();
        match SpoolWriter::create(&path, self.header()) {
            Ok(writer) => {
                self.segment = Some(writer);
                true
            }
            Err(_) => false,
        }
    }

    fn header(&self) -> SpoolHeader {
        let mut extra = BTreeMap::new();
        if let Some(id) = self.run_id {
            // The daemon accepted this run and then lost contact with it; this file is more
            // of that run, not a new one.
            extra.insert("continues_run".to_string(), id.to_string());
        }
        SpoolHeader {
            version: SPOOL_VERSION,
            stamp: self.stamp.clone(),
            started_at: self.started_at,
            exit_code: None,
            signal: None,
            extra,
        }
    }

    /// Finishes the open segment, leaving it for a marker or for the daemon's sweep.
    fn close_segment(&mut self) {
        let Some(mut segment) = self.segment.take() else { return };
        let header = self.header();
        let path = segment.path.clone();
        let bytes = segment.bytes_written;
        if segment.finish_with(header).is_ok() {
            if let Ok(lock) = segment.into_file() {
                if matches!(self.link, Link::Up(_)) {
                    self.send_marker(&path);
                }
                self.closed.push_back(ClosedSegment { path, bytes, _lock: lock });
            }
        }
    }

    fn send_marker(&mut self, path: &Path) {
        let path = path.to_string_lossy().into_owned();
        self.send_control(&Request::IngestSegment { path });
    }

    fn send_control(&mut self, request: &Request) {
        let Ok(frame) = Frame::json(request) else { return };
        let mut bytes = Vec::new();
        if frame.write_to(&mut bytes).is_ok() {
            self.out_bytes += bytes.len();
            self.out.push_back(Item { bytes, kind: Kind::Control });
        }
    }

    fn enforce_budget(&mut self) {
        while self.closed.iter().map(|c| c.bytes).sum::<u64>() > SPOOL_BUDGET {
            self.evict_oldest();
        }
    }

    /// A marker for an evicted segment is harmless: the daemon skips a missing file.
    fn evict_oldest(&mut self) {
        if let Some(oldest) = self.closed.pop_front() {
            let _ = std::fs::remove_file(&oldest.path);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Once the socket has caught up, hands the open segment over and returns to live.
    fn settle_segment(&mut self) {
        if matches!(self.link, Link::Up(_)) && self.segment.is_some() && self.out_bytes < OUT_BYTES / 2 {
            self.close_segment();
        }
    }

    fn flush_segment(&mut self) {
        if let Some(segment) = self.segment.as_mut() {
            let _ = segment.flush();
        }
    }

    fn poll_link(&mut self) {
        match &self.link {
            Link::Connecting(rx) => match rx.try_recv() {
                Ok(contact) => self.apply(contact),
                Err(crossbeam_channel::TryRecvError::Empty) => {}
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.lose_link(),
            },
            Link::Down { retry_at } if Instant::now() >= *retry_at => {
                self.attempts += 1;
                let resume = self.run_id.map(|id| (id, self.started_at));
                let start = self.attempts.is_multiple_of(START_DAEMON_EVERY);
                self.link = Link::Connecting(reconnect(self.stamp.clone(), resume, start));
            }
            _ => {}
        }
    }

    fn apply(&mut self, contact: Contact) {
        if let Some(stamp) = contact.stamp {
            self.stamp = stamp;
        }
        match contact.link {
            Some((stream, run_id)) => {
                self.run_id = Some(run_id);
                self.link = Link::Up(stream);
                // Anything spooled while out of contact goes first, in order.
                let closed: Vec<PathBuf> = self
                    .closed
                    .iter()
                    .map(|c| c.path.clone())
                    .filter(|p| p.exists())
                    .collect();
                for path in closed {
                    self.send_marker(&path);
                }
            }
            None => self.lose_link(),
        }
    }

    /// Stops using the socket. Whatever it had not taken moves to disk, and the front frame
    /// goes back in whole: the daemon discards a frame it only saw part of.
    fn lose_link(&mut self) {
        self.link = Link::Down { retry_at: Instant::now() + RECONNECT_EVERY };
        self.head_sent = 0;
        self.out_bytes = 0;
        for item in std::mem::take(&mut self.out) {
            if item.kind == Kind::Data {
                let mut slice = item.bytes.as_slice();
                if let Ok(Some(frame)) = Frame::read_from(&mut slice) {
                    self.spool(&frame);
                }
            }
        }
    }

    /// Writes as much as the socket will take without waiting.
    fn write_socket(&mut self) {
        let Link::Up(stream) = &mut self.link else { return };
        while !self.out.is_empty() {
            let slices: Vec<IoSlice> = self
                .out
                .iter()
                .take(WRITE_BATCH)
                .enumerate()
                .map(|(i, item)| {
                    IoSlice::new(if i == 0 { &item.bytes[self.head_sent..] } else { &item.bytes })
                })
                .collect();
            match stream.write_vectored(&slices) {
                Ok(0) => return self.lose_link(),
                Ok(mut n) => {
                    while n > 0 {
                        let front_left = self.out[0].bytes.len() - self.head_sent;
                        if n < front_left {
                            self.head_sent += n;
                            break;
                        }
                        n -= front_left;
                        self.head_sent = 0;
                        if let Some(item) = self.out.pop_front() {
                            self.out_bytes -= item.bytes.len();
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return self.lose_link(),
            }
        }
    }

    fn finish(mut self, exit_code: Option<i32>, signal: Option<String>) {
        if let Link::Connecting(rx) = &self.link {
            let contact = rx.recv_timeout(HANDSHAKE_GRACE);
            match contact {
                Ok(contact) => self.apply(contact),
                Err(_) => self.lose_link(),
            }
        }

        if matches!(self.link, Link::Up(_)) {
            self.close_segment();
            if let Some(run_id) = self.run_id {
                let signal = signal.clone();
                self.send_control(&Request::FinishRun { run_id, exit_code, signal });
            }
            let deadline = Instant::now() + FINISH_GRACE;
            while Instant::now() < deadline {
                self.write_socket();
                if !matches!(self.link, Link::Up(_)) {
                    break;
                }
                if self.out.is_empty() {
                    // Delivered. The daemon deletes any segments as it replays them.
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            if matches!(self.link, Link::Up(_)) {
                // Dropping the socket makes the daemon close the run; the spool below
                // resumes it, in order, once that has happened.
                self.lose_link();
            }
        } else {
            self.lose_link();
        }

        // Everything left, under the run's outcome.
        let mut header = self.header();
        header.exit_code = exit_code;
        header.signal = signal;
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            header.extra.insert("dropped_chunks".into(), dropped.to_string());
        }
        if self.segment.is_none() {
            self.open_segment();
        }
        if let Some(mut segment) = self.segment.take() {
            let _ = segment.finish_with(header);
        }
    }
}

// ---------------------------------------------------------------------------
// Spool files
// ---------------------------------------------------------------------------

/// `~/.logbear/spool/<token>-<n>.spool`
///
/// ```text
/// <SpoolHeader as JSON><padding spaces>\n
/// [frame][frame][frame]...
/// ```
///
/// The header is the first line, but its run id, exit code and signal are only known later,
/// so it is written padded to a reserved length and rewritten in place at the end. The file
/// length never changes and the frames never move; a reader that does `read_line` then
/// `serde_json::from_str` is unaffected, because JSON permits trailing whitespace.
///
/// Created under a temporary name and locked before it is renamed, so the daemon's sweep can
/// never see a file with no lock on it and no header in it. A run's segments are numbered
/// and sort in the order they were written.
pub struct SpoolWriter {
    file: BufWriter<std::fs::File>,
    header: SpoolHeader,
    reserve: usize,
    path: PathBuf,
    bytes_written: u64,
}

/// Room to grow into when the header gains the full stamp, which holds every environment
/// variable name and lockfile hash.
const HEADER_SLACK: usize = 64 * 1024;

impl SpoolWriter {
    pub fn create(path: &Path, header: SpoolHeader) -> std::io::Result<SpoolWriter> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("tmp");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            // It holds raw output until the daemon has masked and stored it.
            .mode(0o600)
            .open(&temporary)?;
        lock_exclusive(&file);

        let encoded = serde_json::to_vec(&header)?;
        let reserve = encoded.len() + HEADER_SLACK;
        let mut file = BufWriter::new(file);
        write_padded_header(&mut file, &encoded, reserve)?;
        file.flush()?;
        std::fs::rename(&temporary, path)?;
        Ok(SpoolWriter {
            file,
            header,
            reserve,
            path: path.to_path_buf(),
            bytes_written: 0,
        })
    }

    pub fn write_frame(&mut self, frame: &Frame) -> std::io::Result<()> {
        frame.write_to(&mut self.file)?;
        if let Frame::Data { bytes, .. } = frame {
            self.bytes_written += bytes.len() as u64;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }

    /// Writes the final header. If it outgrew its reserve the earlier one stays on disk: a
    /// missing exit code beats a corrupt file.
    pub fn finish_with(&mut self, header: SpoolHeader) -> std::io::Result<()> {
        self.header = header;
        self.file.flush()?;
        let encoded = serde_json::to_vec(&self.header)?;
        if encoded.len() < self.reserve {
            let file = self.file.get_mut();
            file.seek(SeekFrom::Start(0))?;
            write_padded_header(file, &encoded, self.reserve)?;
            file.seek(SeekFrom::End(0))?;
        }
        self.file.flush()
    }

    fn into_file(self) -> std::io::Result<std::fs::File> {
        self.file.into_inner().map_err(|e| e.into_error())
    }
}

fn write_padded_header<W: Write>(w: &mut W, encoded: &[u8], reserve: usize) -> std::io::Result<()> {
    w.write_all(encoded)?;
    let pad = vec![b' '; reserve.saturating_sub(encoded.len() + 1)];
    w.write_all(&pad)?;
    w.write_all(b"\n")
}

fn lock_exclusive(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// A run identifier that does not need the daemon: time, pid and address-space entropy,
/// enough to be unique among the spool files on one machine.
pub fn local_token() -> String {
    let now = logbear_core::now_micros() as u128;
    let pid = std::process::id() as u128;
    let addr = &now as *const u128 as usize as u128;
    format!("{:016x}{:08x}{:08x}", now, pid as u32, addr as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use logbear_core::proto::Frame;
    use std::io::{BufRead, BufReader, Read};

    /// `LOGBEAR_HOME` is process-global.
    static HOME: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
            let mut header = sample_header();
            header.exit_code = Some(3);
            w.finish_with(header).unwrap();
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
        let mut header = sample_header();
        header.exit_code = Some(0);
        header.signal = Some("SIGTERM".into());
        header.extra.insert("dropped_chunks".into(), "7".into());
        w.finish_with(header).unwrap();
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

    fn test_sink() -> Sink {
        let (tx, _rx) = unbounded();
        std::mem::forget(_rx);
        let (_done_tx, done) = bounded(1);
        Sink {
            tx,
            queued: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            done,
        }
    }

    #[test]
    fn a_burst_of_short_lines_is_queued_not_dropped() {
        let sink = test_sink();
        // Nothing draining at all: tens of thousands of lines still fit the budget.
        for i in 0..50_000 {
            sink.offer(i, b"[info] request handled in 12ms\r\n");
        }
        assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_queue_over_its_byte_budget_drops_and_counts_it() {
        let sink = test_sink();
        let chunk = vec![b'x'; 64 * 1024];
        for i in 0..(QUEUE_BYTES / chunk.len() + 1) {
            sink.offer(i as i64, &chunk);
        }
        assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(sink.queued.load(Ordering::Relaxed), QUEUE_BYTES);
    }

    /// A sender on the far end of a socket nobody reads, with its own spool directory.
    fn stalled_sender(dir: &Path) -> (Sender, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let sender = Sender {
            link: Link::Up(ours),
            out: VecDeque::new(),
            out_bytes: 0,
            head_sent: 0,
            segment: None,
            closed: VecDeque::new(),
            next_segment: 1,
            token: dir.file_name().unwrap().to_string_lossy().into_owned(),
            stamp: RunStamp::default(),
            started_at: 1,
            run_id: Some(9),
            queued: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            attempts: 0,
        };
        (sender, theirs)
    }

    fn data(n: usize) -> Msg {
        Msg::Data { captured_at: n as i64, bytes: vec![b'x'; 64 * 1024] }
    }

    #[test]
    fn a_socket_that_stops_taking_bytes_overflows_to_disk_without_losing_any() {
        let _guard = HOME.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", dir.path());
        let (mut sender, _theirs) = stalled_sender(dir.path());
        sender.queued.store(usize::MAX / 2, Ordering::Relaxed);

        let total = OUT_BYTES / (64 * 1024) * 3;
        for n in 0..total {
            sender.accept(data(n));
            sender.write_socket();
            sender.settle_segment();
        }

        assert!(sender.out_bytes <= OUT_BYTES + 64 * 1024, "memory is unbounded");
        assert!(sender.segment.is_some(), "the overflow did not reach disk");
        assert_eq!(sender.dropped.load(Ordering::Relaxed), 0);
        std::env::remove_var("LOGBEAR_HOME");
    }

    #[test]
    fn a_lost_socket_moves_its_backlog_to_disk_whole() {
        let _guard = HOME.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", dir.path());
        let (mut sender, theirs) = stalled_sender(dir.path());
        sender.queued.store(usize::MAX / 2, Ordering::Relaxed);
        for n in 0..8 {
            sender.accept(data(n));
        }
        sender.head_sent = 10; // A frame the socket took part of.
        drop(theirs);
        sender.lose_link();

        assert!(sender.out.is_empty());
        let mut segment = sender.segment.take().unwrap();
        segment.flush().unwrap();
        let mut reader = BufReader::new(std::fs::File::open(&segment.path).unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut frames = 0;
        while let Some(Frame::Data { captured_at, .. }) = Frame::read_from(&mut reader).unwrap() {
            assert_eq!(captured_at, frames, "frames came back out of order");
            frames += 1;
        }
        assert_eq!(frames, 8);
        std::env::remove_var("LOGBEAR_HOME");
    }

    #[test]
    fn a_segment_is_handed_over_in_band_once_the_socket_catches_up() {
        let _guard = HOME.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LOGBEAR_HOME", dir.path());
        let (mut sender, _theirs) = stalled_sender(dir.path());
        sender.spool(&Frame::Data { captured_at: 1, bytes: b"burst\n".to_vec() });
        sender.settle_segment();

        assert!(sender.segment.is_none());
        assert_eq!(sender.closed.len(), 1);
        let item = sender.out.front().expect("a marker was queued");
        let mut slice = item.bytes.as_slice();
        let frame = Frame::read_from(&mut slice).unwrap().unwrap();
        match frame.decode_json::<Request>().unwrap() {
            Request::IngestSegment { path } => {
                assert_eq!(Path::new(&path), sender.closed[0].path);
            }
            _ => panic!("expected a segment marker"),
        }
        std::env::remove_var("LOGBEAR_HOME");
    }

    #[test]
    fn an_old_daemon_is_only_replaced_once_in_a_while() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("daemon-replaced");
        let every = Duration::from_secs(60);
        assert!(may_replace_daemon(&marker, every));
        assert!(!may_replace_daemon(&marker, every), "a restart loop would cut off every run");
        assert!(may_replace_daemon(&marker, Duration::ZERO));
    }

    #[test]
    fn segments_sort_in_the_order_they_were_written() {
        assert!(format!("{}-{:04}.spool", "t", 9) < format!("{}-{:04}.spool", "t", 10));
    }

    #[test]
    fn a_spool_is_never_visible_without_its_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.spool");
        let w = SpoolWriter::create(&path, sample_header()).unwrap();
        assert!(!path.with_extension("tmp").exists());
        let mut reader = BufReader::new(std::fs::File::open(&path).unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str::<SpoolHeader>(&line).unwrap();
        drop(w);
    }
}

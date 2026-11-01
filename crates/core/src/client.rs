//! Synchronous client for the daemon socket, shared by every binary and the Tauri backend.
//!
//! Blocking and thread-per-connection: the work is a local unix socket round trip against
//! a local SQLite database, so an async runtime would gain nothing and would impose itself
//! on every consumer.

use crate::error::{Error, Result};
use crate::model::*;
use crate::paths;
use crate::proto::*;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    socket: PathBuf,
}

impl Client {
    pub fn connect() -> Result<Client> {
        Client::connect_to(&paths::socket())
    }

    pub fn connect_to(socket: &Path) -> Result<Client> {
        let stream = UnixStream::connect(socket)
            .map_err(|_| Error::DaemonUnreachable(socket.to_path_buf()))?;
        let read_half = stream.try_clone()?;
        Ok(Client {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(stream),
            socket: socket.to_path_buf(),
        })
    }

    /// Connects, starting the daemon first if nothing is listening.
    pub fn connect_or_start() -> Result<Client> {
        match Client::connect() {
            Ok(c) => Ok(c),
            Err(_) => {
                spawn_daemon()?;
                // Poll while the daemon creates, binds and listens on the socket.
                for _ in 0..100 {
                    std::thread::sleep(Duration::from_millis(20));
                    if let Ok(c) = Client::connect() {
                        return Ok(c);
                    }
                }
                Err(Error::DaemonUnreachable(paths::socket()))
            }
        }
    }

    pub fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.reader.get_ref().set_read_timeout(timeout)?;
        Ok(())
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Sends a request and decodes the reply into `T`.
    pub fn call<T: for<'de> serde::Deserialize<'de>>(&mut self, req: &Request) -> Result<T> {
        let value = self.call_value(req)?;
        Ok(serde_json::from_value(value)?)
    }

    pub fn call_value(&mut self, req: &Request) -> Result<serde_json::Value> {
        self.send(&Frame::json(req)?)?;
        let frame = self
            .recv()?
            .ok_or_else(|| Error::Protocol("daemon closed the connection".into()))?;
        frame.decode_json::<Response>()?.into_result()
    }

    pub fn send(&mut self, frame: &Frame) -> Result<()> {
        frame.write_to(&mut self.writer)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Queues a frame without flushing, so capture costs one syscall per batch rather
    /// than one per read.
    pub fn queue(&mut self, frame: &Frame) -> Result<()> {
        frame.write_to(&mut self.writer)?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }

    pub fn recv(&mut self) -> Result<Option<Frame>> {
        Frame::read_from(&mut self.reader)
    }

    // ---- convenience wrappers over `call` ----

    pub fn ping(&mut self) -> Result<()> {
        self.call::<serde_json::Value>(&Request::Ping).map(|_| ())
    }

    pub fn status(&mut self) -> Result<DaemonStatus> {
        self.call(&Request::Status)
    }

    pub fn projects(&mut self) -> Result<Vec<Project>> {
        self.call(&Request::ListProjects)
    }

    pub fn project_status(&mut self, project: ProjectRef) -> Result<ProjectStatus> {
        self.call(&Request::ProjectStatus { project })
    }

    pub fn runs(&mut self, project: ProjectRef, limit: usize, offset: usize) -> Result<Vec<Run>> {
        self.call(&Request::ListRuns {
            project,
            limit,
            offset,
        })
    }

    pub fn entries(&mut self, project: ProjectRef, filter: EntryFilter) -> Result<Vec<EntryView>> {
        self.call(&Request::Entries { project, filter })
    }

    pub fn query(
        &mut self,
        project: ProjectRef,
        sql: &str,
        params: Vec<serde_json::Value>,
        max_rows: Option<usize>,
    ) -> Result<QueryResult> {
        self.call(&Request::Query {
            project,
            sql: sql.to_string(),
            params,
            max_rows,
        })
    }

    pub fn schema(&mut self, project: ProjectRef) -> Result<String> {
        self.call(&Request::Schema { project })
    }

    pub fn diff(
        &mut self,
        project: ProjectRef,
        a: Option<RunRef>,
        b: Option<RunRef>,
    ) -> Result<RunDiff> {
        self.call(&Request::Diff { project, a, b })
    }

    pub fn unread(
        &mut self,
        project: ProjectRef,
        session_id: &str,
        harness: &str,
    ) -> Result<UnreadState> {
        self.call(&Request::Unread {
            project,
            session_id: session_id.to_string(),
            harness: harness.to_string(),
        })
    }

    pub fn ack(
        &mut self,
        project: ProjectRef,
        session_id: &str,
        harness: &str,
        entry_id: Option<EntryId>,
        kind: AckKind,
    ) -> Result<UnreadState> {
        self.call(&Request::Ack {
            project,
            session_id: session_id.to_string(),
            harness: harness.to_string(),
            entry_id,
            kind,
        })
    }

    /// Blocks until the next matching entry arrives or `timeout_ms` elapses.
    pub fn wait(
        &mut self,
        project: ProjectRef,
        timeout_ms: u64,
        level: Option<Level>,
        after_entry: Option<EntryId>,
    ) -> Result<Vec<EntryView>> {
        // The socket deadline must exceed the daemon's own budget, so an expiry comes back
        // as an empty result rather than a broken connection.
        self.set_timeout(Some(Duration::from_millis(timeout_ms + 5_000)))?;
        let out = self.call(&Request::Wait {
            project,
            timeout_ms,
            level,
            after_entry,
        });
        let _ = self.set_timeout(None);
        out
    }

    /// Streams a run's stored bytes, calling `sink` for each data frame in order.
    pub fn raw_stream(
        &mut self,
        project: ProjectRef,
        run_id: RunId,
        sink: impl FnMut(&[u8]) -> std::io::Result<()>,
    ) -> Result<()> {
        self.raw_tail(project, run_id, None, sink)
    }

    /// [`Client::raw_stream`] limited to the last `tail_lines` lines, or all of them.
    pub fn raw_tail(
        &mut self,
        project: ProjectRef,
        run_id: RunId,
        tail_lines: Option<i64>,
        mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
    ) -> Result<()> {
        self.send(&Frame::json(&Request::RawStream {
            project,
            run_id,
            start_line: None,
            end_line: None,
            tail_lines,
        })?)?;
        loop {
            match self.recv()? {
                Some(Frame::Data { bytes, .. }) => sink(&bytes)?,
                Some(frame @ Frame::Json(_)) => {
                    return frame.decode_json::<Response>()?.into_result().map(|_| ())
                }
                None => return Err(Error::Protocol("stream ended without a reply".into())),
            }
        }
    }

    /// Turns this connection into an event subscription. Consumes the client, since no
    /// further requests can be sent on it.
    pub fn subscribe(mut self, project: Option<ProjectRef>) -> Result<Subscription> {
        self.send(&Frame::json(&Request::Subscribe { project })?)?;
        // The daemon acknowledges before the first event.
        match self.recv()? {
            Some(frame @ Frame::Json(_)) => {
                frame.decode_json::<Response>()?.into_result()?;
            }
            _ => return Err(Error::Protocol("subscribe was not acknowledged".into())),
        }
        Ok(Subscription { client: self })
    }
}

pub struct Subscription {
    client: Client,
}

impl Subscription {
    /// Blocks for the next event. `Ok(None)` means the daemon went away, which is a
    /// disconnection rather than an error.
    // Not `Iterator`: the item is `Result<Option<Event>>`, so the trait would have to
    // collapse a dead socket and a decode failure into the same `None`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Event>> {
        loop {
            match self.client.recv()? {
                Some(frame @ Frame::Json(_)) => return Ok(Some(frame.decode_json::<Event>()?)),
                Some(Frame::Data { .. }) => continue,
                None => return Ok(None),
            }
        }
    }
}

/// Locates `logbeard`, preferring the copy beside the current executable so a bundled app
/// finds its own daemon rather than one on `PATH`.
pub fn daemon_binary() -> Option<PathBuf> {
    // An explicit override is authoritative, including when it names nothing: falling back
    // to a discovered daemon would turn a typo into a silently different binary, and leaves
    // a test no way to say "there is no daemon here".
    if let Some(p) = std::env::var_os("LOGBEAR_DAEMON").map(PathBuf::from) {
        return p.exists().then_some(p);
    }
    // Canonicalise before looking for a sibling: on macOS `current_exe()` reports the
    // launch path, so reaching `logbear` through the installer's `~/.local/bin` symlink
    // would search that directory instead of the bundle the daemon lives in.
    if let Ok(exe) = std::env::current_exe() {
        let resolved = exe.canonicalize().unwrap_or(exe);
        if let Some(dir) = resolved.parent() {
            for candidate in [dir.join("logbeard"), dir.join("../logbeard")] {
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }
    // The app bundle, for when `logbear` was copied rather than linked.
    for bundle in ["/Applications/logbear.app", "~/Applications/logbear.app"] {
        let expanded = if let Some(rest) = bundle.strip_prefix("~/") {
            match std::env::var_os("HOME") {
                Some(h) => PathBuf::from(h).join(rest),
                None => continue,
            }
        } else {
            PathBuf::from(bundle)
        };
        let candidate = expanded.join("Contents/MacOS/logbeard");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    which("logbeard")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Starts the daemon detached, so it outlives the process that spawned it.
pub fn spawn_daemon() -> Result<()> {
    use std::process::{Command, Stdio};
    let bin = daemon_binary()
        .ok_or_else(|| Error::other("logbeard was not found next to logbear or on PATH"))?;
    paths::ensure_home().map_err(|e| Error::io(paths::home(), e))?;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::daemon_log())
        .map_err(|e| Error::io(paths::daemon_log(), e))?;
    let err = log.try_clone()?;

    let mut cmd = Command::new(bin);
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));

    // Its own session, or it inherits the terminal's foreground process group and every
    // Ctrl-C meant for the wrapped command takes the daemon down with it — which looks like
    // a daemon that keeps dying, and leaves the next invocation to start another one.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }

    cmd.spawn().map_err(|e| Error::io(paths::home(), e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_daemon_is_found_through_the_symlink_the_installer_places_on_path() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("Contents/MacOS");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real_exe = real_dir.join("logbear");
        std::fs::write(&real_exe, b"#!/bin/sh\n").unwrap();
        std::fs::write(real_dir.join("logbeard"), b"#!/bin/sh\n").unwrap();

        let link_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("logbear");
        std::os::unix::fs::symlink(&real_exe, &link).unwrap();

        // Both sides are canonicalised: on macOS /var is itself a symlink to /private/var,
        // so comparing a resolved path against an unresolved one fails for the wrong reason.
        let resolved = link.canonicalize().unwrap();
        assert_eq!(
            resolved.parent().unwrap().join("logbeard"),
            real_dir.canonicalize().unwrap().join("logbeard")
        );
        assert!(
            !link_dir.join("logbeard").exists(),
            "the unresolved path must not find it, or this test proves nothing"
        );
    }
}

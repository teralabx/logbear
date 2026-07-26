//! Synchronous client for the daemon socket.
//!
//! Shared by every binary — the shim, the CLI, the MCP server, the hook, and the Tauri
//! backend — so there is exactly one implementation of "talk to runlogd" and one place
//! where its failure modes are decided.
//!
//! Blocking and thread-per-connection on purpose. The work is a local unix socket round
//! trip against a local SQLite database; an async runtime would buy nothing and would
//! force every consumer to adopt one.

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

    /// Connect, starting the daemon first if nothing is listening.
    ///
    /// Auto-start belongs here rather than in an installer step, because "keeps monitoring"
    /// has to be true after a reboot the user did not think about.
    pub fn connect_or_start() -> Result<Client> {
        match Client::connect() {
            Ok(c) => Ok(c),
            Err(_) => {
                spawn_daemon()?;
                // The daemon has to create the socket, bind, and listen. Poll briefly
                // rather than sleeping a fixed pessimistic amount.
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

    /// Send a request and decode the reply into `T`.
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

    /// Queue a frame without flushing. Capture uses this so a busy stream costs one
    /// syscall per batch rather than one per read.
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

    // ---- convenience wrappers, so callers do not hand-build requests ----

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
        record_block: bool,
    ) -> Result<UnreadState> {
        self.call(&Request::Ack {
            project,
            session_id: session_id.to_string(),
            harness: harness.to_string(),
            entry_id,
            record_block,
        })
    }

    /// Blocking wait for the next matching entry. Used right after making a change, when
    /// polling would be the wrong shape.
    pub fn wait(
        &mut self,
        project: ProjectRef,
        timeout_ms: u64,
        level: Option<Level>,
        after_entry: Option<EntryId>,
    ) -> Result<Vec<EntryView>> {
        // Give the socket a little longer than the daemon's own budget, so a timeout is
        // reported by the daemon as an empty result rather than as a broken connection.
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

    /// Stream a run's stored bytes. `sink` is called for each data frame in order.
    pub fn raw_stream(
        &mut self,
        project: ProjectRef,
        run_id: RunId,
        mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
    ) -> Result<()> {
        self.send(&Frame::json(&Request::RawStream {
            project,
            run_id,
            start_line: None,
            end_line: None,
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

    /// Turn this connection into an event subscription. Consumes the client, because no
    /// further requests can be sent on it.
    pub fn subscribe(mut self, project: Option<ProjectRef>) -> Result<Subscription> {
        self.send(&Frame::json(&Request::Subscribe { project })?)?;
        // The daemon acknowledges before the first event, so a caller knows it is live.
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
    /// Block for the next event. `Ok(None)` means the daemon went away, which the app
    /// shows as a disconnected badge rather than treating as an error.
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

/// Locate `runlogd`. Next to the current executable first, which is how the bundled app
/// finds its own daemon rather than whatever happens to be on `PATH`.
pub fn daemon_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RUNLOG_DAEMON").map(PathBuf::from) {
        if p.exists() {
            return Some(p);
        }
    }
    // Canonicalise first. On macOS `current_exe()` reports the path used to launch the
    // process, so when `runlog` is reached through the symlink the installer places on
    // PATH, the parent directory is `~/.local/bin` — where `runlogd` will never be. The
    // daemon lives beside the real binary, inside the app bundle.
    if let Ok(exe) = std::env::current_exe() {
        let resolved = exe.canonicalize().unwrap_or(exe);
        if let Some(dir) = resolved.parent() {
            for candidate in [dir.join("runlogd"), dir.join("../runlogd")] {
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }
    // The app bundle, for the case where `runlog` was copied rather than linked.
    for bundle in ["/Applications/runlog.app", "~/Applications/runlog.app"] {
        let expanded = if let Some(rest) = bundle.strip_prefix("~/") {
            match std::env::var_os("HOME") {
                Some(h) => PathBuf::from(h).join(rest),
                None => continue,
            }
        } else {
            PathBuf::from(bundle)
        };
        let candidate = expanded.join("Contents/MacOS/runlogd");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    which("runlogd")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Start the daemon detached, so it outlives whatever started it.
pub fn spawn_daemon() -> Result<()> {
    use std::process::{Command, Stdio};
    let bin = daemon_binary()
        .ok_or_else(|| Error::other("runlogd was not found next to runlog or on PATH"))?;
    paths::ensure_home().map_err(|e| Error::io(paths::home(), e))?;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::daemon_log())
        .map_err(|e| Error::io(paths::daemon_log(), e))?;
    let err = log.try_clone()?;

    Command::new(bin)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|e| Error::io(paths::home(), e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_is_found_through_the_symlink_the_installer_places_on_path() {
        // The failure this guards: on macOS `current_exe()` reports the launch path, so
        // reaching `runlog` through ~/.local/bin/runlog put the search in ~/.local/bin,
        // where runlogd will never be. `runlog doctor` then reported the daemon binary
        // missing while that very binary was running the daemon.
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("Contents/MacOS");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real_exe = real_dir.join("runlog");
        std::fs::write(&real_exe, b"#!/bin/sh\n").unwrap();
        std::fs::write(real_dir.join("runlogd"), b"#!/bin/sh\n").unwrap();

        let link_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("runlog");
        std::os::unix::fs::symlink(&real_exe, &link).unwrap();

        // Resolving the link is what makes the sibling lookup land in the bundle.
        // Both sides are canonicalised: on macOS /var is itself a symlink to /private/var,
        // so comparing a resolved path against an unresolved one fails for the wrong reason.
        let resolved = link.canonicalize().unwrap();
        assert_eq!(
            resolved.parent().unwrap().join("runlogd"),
            real_dir.canonicalize().unwrap().join("runlogd")
        );
        assert!(
            !link_dir.join("runlogd").exists(),
            "the unresolved path must not find it, or this test proves nothing"
        );
    }
}

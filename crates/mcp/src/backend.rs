//! Everything the tools need from the daemon, behind one trait.
//!
//! Two reasons this is a trait rather than a bare [`Client`]. First, the protocol layer is
//! worth testing without a running daemon — the JSON-RPC framing, the banner, and the error
//! shapes are where the bugs live, and none of them need real data. Second, it puts every
//! reconnect decision in one place: an MCP server is long-lived, the daemon it talks to is
//! restartable, and a session that dies because runlogd was upgraded mid-conversation would
//! be indistinguishable from runlog being broken.

use runlog_core::client::Client;
use runlog_core::model::{EntryId, EntryView, Level};
use runlog_core::proto::{ProjectRef, QueryResult, Request, RunDiff, RunRef, UnreadState};
use runlog_core::Result;
use serde_json::Value;

/// The harness string recorded against this server's session cursor.
pub const HARNESS: &str = "mcp";

pub trait Backend {
    fn unread(&mut self, project: ProjectRef, session_id: &str) -> Result<UnreadState>;

    /// `Request::Schema` returns prose composed by the daemon (DDL, views, functions,
    /// worked examples, delimiter mode, retention). It arrives as an opaque value so a
    /// richer shape later does not break this crate.
    fn schema(&mut self, project: ProjectRef) -> Result<Value>;

    fn query(
        &mut self,
        project: ProjectRef,
        sql: &str,
        params: Vec<Value>,
        max_rows: Option<usize>,
    ) -> Result<QueryResult>;

    fn wait(
        &mut self,
        project: ProjectRef,
        timeout_ms: u64,
        level: Option<Level>,
    ) -> Result<Vec<EntryView>>;

    fn diff(
        &mut self,
        project: ProjectRef,
        a: Option<RunRef>,
        b: Option<RunRef>,
    ) -> Result<RunDiff>;

    fn ack(
        &mut self,
        project: ProjectRef,
        session_id: &str,
        entry_id: Option<EntryId>,
    ) -> Result<UnreadState>;
}

pub struct DaemonBackend {
    client: Option<Client>,
}

impl DaemonBackend {
    pub fn new() -> DaemonBackend {
        DaemonBackend { client: None }
    }

    /// Run `f` against a live connection, reconnecting once if the socket has gone away.
    ///
    /// One retry, not a loop: if a second attempt against a freshly opened socket also
    /// fails, the problem is the request, and retrying it would only turn a clear error
    /// into a hang.
    fn with_client<T>(&mut self, mut f: impl FnMut(&mut Client) -> Result<T>) -> Result<T> {
        if self.client.is_none() {
            self.client = Some(Client::connect_or_start()?);
        }
        let first = f(self.client.as_mut().expect("connected above"));
        match first {
            Ok(v) => Ok(v),
            Err(e) if is_transport_failure(&e) => {
                self.client = None;
                let mut fresh = Client::connect_or_start()?;
                let out = f(&mut fresh);
                self.client = Some(fresh);
                out
            }
            Err(e) => Err(e),
        }
    }
}

impl Default for DaemonBackend {
    fn default() -> Self {
        DaemonBackend::new()
    }
}

/// A dead socket is worth retrying; a rejected query is not. Getting this wrong in the
/// permissive direction means re-running a five-second query on every failure.
fn is_transport_failure(e: &runlog_core::Error) -> bool {
    use runlog_core::Error::*;
    matches!(e, DaemonUnreachable(_) | Protocol(_) | Bare(_) | Io { .. })
}

impl Backend for DaemonBackend {
    fn unread(&mut self, project: ProjectRef, session_id: &str) -> Result<UnreadState> {
        self.with_client(|c| c.unread(project.clone(), session_id, HARNESS))
    }

    fn schema(&mut self, project: ProjectRef) -> Result<Value> {
        self.with_client(|c| {
            c.call_value(&Request::Schema {
                project: project.clone(),
            })
        })
    }

    fn query(
        &mut self,
        project: ProjectRef,
        sql: &str,
        params: Vec<Value>,
        max_rows: Option<usize>,
    ) -> Result<QueryResult> {
        self.with_client(|c| c.query(project.clone(), sql, params.clone(), max_rows))
    }

    fn wait(
        &mut self,
        project: ProjectRef,
        timeout_ms: u64,
        level: Option<Level>,
    ) -> Result<Vec<EntryView>> {
        self.with_client(|c| c.wait(project.clone(), timeout_ms, level, None))
    }

    fn diff(
        &mut self,
        project: ProjectRef,
        a: Option<RunRef>,
        b: Option<RunRef>,
    ) -> Result<RunDiff> {
        self.with_client(|c| c.diff(project.clone(), a.clone(), b.clone()))
    }

    fn ack(
        &mut self,
        project: ProjectRef,
        session_id: &str,
        entry_id: Option<EntryId>,
    ) -> Result<UnreadState> {
        self.with_client(|c| c.ack(project.clone(), session_id, HARNESS, entry_id, false))
    }
}

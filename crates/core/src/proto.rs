//! The daemon wire protocol.
//!
//! One unix socket at `~/.logbear/logbeard.sock` serves three kinds of client: the shim
//! streaming capture, request/response callers (CLI, MCP, hook, Tauri), and long-lived
//! subscribers. They are distinguished by what they send first, not by separate sockets.
//!
//! Frames are a one-byte kind, a length, and a payload rather than JSON throughout, since
//! base64-ing every chunk of captured output would dominate the system's cost.
//!
//! ```text
//! [u8 kind][u32 len LE][payload len bytes]
//!
//! kind 1  Json     serde_json message (Request | Response | Event)
//! kind 2  Data     [i64 captured_at LE][raw pty bytes]   — ingest connections only
//! ```

use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};

pub const FRAME_JSON: u8 = 1;
pub const FRAME_DATA: u8 = 2;

/// Upper bound on a frame payload, so a bad length claim cannot force a huge allocation.
pub const MAX_FRAME: usize = 32 * 1024 * 1024;

/// How a caller names a project. Every variant resolves to a canonical git root.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "by", content = "value")]
pub enum ProjectRef {
    /// A directory, resolved up to its git root and then matched by containment so a
    /// server started in `apps/api` is found from the repo root.
    Path(String),
    Id(ProjectId),
}

impl ProjectRef {
    pub fn cwd() -> ProjectRef {
        ProjectRef::Path(
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".into()),
        )
    }
}

/// Which run to compare against in a diff. `LastGood` is the default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum RunRef {
    Id(RunId),
    /// Most recent run in the project, whatever its outcome.
    Latest,
    /// Most recent run that exited 0 and logged no errors.
    LastGood,
    /// Most recent run that did not meet that bar.
    LastBad,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryFilter {
    pub run_id: Option<RunId>,
    pub level: Option<Level>,
    #[serde(default)]
    pub errors_only: bool,
    /// Only entries whose signature is being seen for the first time in this project, at
    /// any severity. Finds ad-hoc `print` / `IO.inspect` debugging, which declares no
    /// level and so cannot be selected by one.
    #[serde(default)]
    pub novel_only: bool,
    /// Unix micros; entries strictly newer than this.
    pub since: Option<i64>,
    /// Entries with an id strictly greater than this; how the live view tails.
    pub after_entry: Option<EntryId>,
    /// FTS5 match expression.
    pub search: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    /// Oldest-first instead of newest-first.
    #[serde(default)]
    pub ascending: bool,
}

fn default_limit() -> usize {
    100
}

impl Default for EntryFilter {
    /// Written out rather than derived, because a derived `limit: 0` would silently
    /// truncate every query built with `..Default::default()`.
    fn default() -> Self {
        EntryFilter {
            run_id: None,
            level: None,
            errors_only: false,
            novel_only: false,
            since: None,
            after_entry: None,
            search: None,
            limit: default_limit(),
            offset: 0,
            ascending: false,
        }
    }
}

/// Two mid-turn interruptions per session. Enough to catch the change that just broke
/// something; few enough that a flapping server cannot hold the agent hostage.
fn default_change_block_budget() -> i64 {
    2
}

/// What an [`Request::Ack`] is acknowledging.
///
/// A session keeps two watermarks because it is asked two different questions. "Which errors
/// are new?" may only advance when errors are reported to the agent; "which lines has the
/// agent not looked at yet?" advances whenever it is told to go look. One watermark serving
/// both would let a nudge silently consume errors the Stop hook exists to surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AckKind {
    /// The agent read these entries. Advances the error cursor, charges nothing.
    #[default]
    Seen,
    /// A Stop block: advance the error cursor and charge the Stop budget.
    StopBlock,
    /// A PostToolUse block: advance the error cursor and charge the change budget.
    ChangeBlock,
    /// A verify nudge: advance the nudge cursor only.
    Nudge,
}

impl AckKind {
    /// Whether this reports errors to the agent, and so must advance the error cursor.
    /// A block whose watermark did not move re-fires on the same errors forever.
    pub fn advances_errors(self) -> bool {
        !matches!(self, AckKind::Nudge)
    }

    pub fn is_block(self) -> bool {
        matches!(self, AckKind::StopBlock | AckKind::ChangeBlock)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum Request {
    Ping,

    // ---- capture (shim) ----
    /// Opens an ingest connection. Every subsequent [`FRAME_DATA`] frame belongs to the
    /// run this returns.
    StartRun {
        stamp: RunStamp,
    },
    FinishRun {
        run_id: RunId,
        exit_code: Option<i32>,
        signal: Option<String>,
    },
    Resize {
        run_id: RunId,
        cols: u16,
        rows: u16,
    },
    /// Hands the daemon a spool file written while it was unreachable (C2).
    IngestSpool {
        path: String,
    },

    // ---- read ----
    Status,
    ListProjects,
    ProjectStatus {
        project: ProjectRef,
    },
    ListRuns {
        project: ProjectRef,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default)]
        offset: usize,
    },
    GetRun {
        project: ProjectRef,
        run_id: RunId,
    },
    Entries {
        project: ProjectRef,
        #[serde(default)]
        filter: EntryFilter,
    },
    /// Verbatim bytes for a line range, used by `logbear cat` and the xterm replay.
    /// Returns [`FRAME_DATA`] frames followed by a `Response::Ok(null)`.
    RawStream {
        project: ProjectRef,
        run_id: RunId,
        start_line: Option<i64>,
        end_line: Option<i64>,
    },
    Fingerprints {
        project: ProjectRef,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default)]
        errors_only: bool,
    },
    /// Read-only SQL, run inside the sandbox described in the design spec §9.4.
    Query {
        project: ProjectRef,
        sql: String,
        #[serde(default)]
        params: Vec<serde_json::Value>,
        max_rows: Option<usize>,
    },
    /// DDL, views, functions and worked examples, for an agent about to write SQL.
    Schema {
        project: ProjectRef,
    },
    Diff {
        project: ProjectRef,
        a: Option<RunRef>,
        b: Option<RunRef>,
    },
    /// Blocks until a matching entry lands or the timeout expires.
    Wait {
        project: ProjectRef,
        timeout_ms: u64,
        level: Option<Level>,
        after_entry: Option<EntryId>,
    },

    // ---- agent session state ----
    /// What this session has not yet been told about. Drives the MCP banner and the hook.
    Unread {
        project: ProjectRef,
        session_id: String,
        harness: String,
    },
    /// Advances one of the session's watermarks.
    Ack {
        project: ProjectRef,
        session_id: String,
        harness: String,
        entry_id: Option<EntryId>,
        /// Which watermark to move, and which budget to charge.
        #[serde(default)]
        kind: AckKind,
    },

    // ---- settings ----
    GetSettings,
    UpdateSettings {
        settings: Settings,
    },
    UpdateProject {
        project: ProjectRef,
        retention_bytes: Option<Option<i64>>,
        push_enabled: Option<bool>,
        notify_enabled: Option<bool>,
    },
    /// Forces a retention sweep now instead of waiting for the timer.
    Vacuum {
        project: ProjectRef,
    },
    /// Drops and rebuilds `entries`, fingerprints and the FTS index from `chunks`.
    /// Non-destructive by C3, so a parser bug is never data loss.
    Reparse {
        project: ProjectRef,
        run_id: Option<RunId>,
    },
    DeleteRun {
        project: ProjectRef,
        run_id: RunId,
    },

    // ---- live ----
    /// Converts this connection into an event stream; no further requests are read.
    Subscribe {
        project: Option<ProjectRef>,
    },

    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum Response {
    Ok { data: serde_json::Value },
    Err { message: String, kind: String },
}

impl Response {
    pub fn ok<T: Serialize>(value: T) -> Response {
        match serde_json::to_value(value) {
            Ok(data) => Response::Ok { data },
            Err(e) => Response::Err {
                message: e.to_string(),
                kind: "serialize".into(),
            },
        }
    }

    pub fn err(kind: &str, message: impl Into<String>) -> Response {
        Response::Err {
            message: message.into(),
            kind: kind.into(),
        }
    }

    pub fn into_result(self) -> crate::Result<serde_json::Value> {
        match self {
            Response::Ok { data } => Ok(data),
            Response::Err { message, .. } => Err(crate::Error::Other(message)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum Event {
    RunStarted { run: Run },
    RunFinished { run: Run },
    Entries { run_id: RunId, entries: Vec<EntryView> },
    Counters { run_id: RunId, counters: Counters },
    /// A notification the daemon posted, mirrored to the app so in-window state matches
    /// the OS banner.
    Notice { notice: Notice },
    ProjectsChanged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub project_id: ProjectId,
    pub project_name: String,
    pub title: String,
    pub body: String,
    pub level: Level,
    pub at: i64,
    /// Set when this notice replaces an earlier one for the same signature, so the OS
    /// banner updates its count instead of stacking.
    pub replaces_key: Option<String>,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub started_at: i64,
    pub socket: String,
    pub projects: usize,
    pub active_runs: usize,
    pub total_db_bytes: i64,
    pub spool_pending: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: usize,
    /// True when the row cap or the byte cap cut the result short.
    pub truncated: bool,
    pub elapsed_ms: u64,
}

/// What an agent session has not been shown yet. Rendered as the banner on every MCP
/// response and as the hook's block reason.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnreadState {
    pub new_errors: i64,
    pub new_signatures: i64,
    pub oldest_at: Option<i64>,
    pub newest_entry_id: Option<EntryId>,
    pub blocks_issued: i64,
    pub blocks_remaining: i64,
    /// PostToolUse blocks left, on their own allowance. Kept apart from `blocks_remaining`
    /// so a noisy turn cannot leave the Stop gate unable to block when it matters most.
    #[serde(default)]
    pub change_blocks_remaining: i64,
    pub push_enabled: bool,
    /// Entries of any level after the nudge cursor. The verify nudge fires on this rather
    /// than on `new_errors`: a change that logged only a clean reload still deserves a look,
    /// and one that logged nothing at all deserves silence.
    #[serde(default)]
    pub unseen_lines: i64,
    /// Whether a run is open for this project. Nothing is worth checking if nothing is
    /// running to produce it.
    #[serde(default)]
    pub server_running: bool,
    /// Ready-to-inject entries, newest last. Capped by the caller's budget.
    pub entries: Vec<EntryView>,
}

impl UnreadState {
    pub fn is_empty(&self) -> bool {
        self.new_errors == 0
    }

    /// One-line banner prefixed to every MCP tool response.
    pub fn banner(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let age = self
            .oldest_at
            .map(|t| {
                let secs = ((crate::now_micros() - t) / 1_000_000).max(0);
                if secs < 90 {
                    format!(" Oldest {secs}s ago.")
                } else {
                    format!(" Oldest {}m ago.", secs / 60)
                }
            })
            .unwrap_or_default();
        let sigs = if self.new_signatures > 0 {
            format!(" ({} new signature{})", self.new_signatures, plural(self.new_signatures))
        } else {
            String::new()
        };
        Some(format!(
            "⚠️ {} new error{} since your last check{sigs}.{age}",
            self.new_errors,
            plural(self.new_errors),
        ))
    }
}

fn plural(n: i64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Default retention cap per project, in bytes.
    pub retention_bytes: i64,
    pub notifications_enabled: bool,
    pub quiet_hours: Option<QuietHours>,
    /// Blocks issued per agent session before push degrades to advisory context.
    pub block_budget: i64,
    /// PostToolUse blocks per session. Deliberately smaller than `block_budget` and charged
    /// separately: interrupting mid-turn is a bigger imposition than interrupting a stop.
    #[serde(default = "default_change_block_budget")]
    pub change_block_budget: i64,
    pub push_enabled: bool,
    /// Commands never captured, being interactive sessions rather than runs.
    pub denylist: Vec<String>,
    pub redaction_enabled: bool,
    pub extra_redaction_patterns: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            retention_bytes: 1024 * 1024 * 1024,
            notifications_enabled: true,
            quiet_hours: None,
            block_budget: 3,
            change_block_budget: default_change_block_budget(),
            push_enabled: true,
            denylist: [
                "psql", "mysql", "sqlite3", "redis-cli", "ssh", "vim", "nvim", "vi", "nano",
                "emacs", "less", "more", "man", "top", "htop", "btop", "tmux", "screen", "gpg",
                "passwd", "sudo",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            redaction_enabled: true,
            extra_redaction_patterns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QuietHours {
    /// Minutes past local midnight.
    pub start_min: u32,
    pub end_min: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDiff {
    pub a: Option<Run>,
    pub b: Option<Run>,
    /// Signatures in B that are absent from A.
    pub new_signatures: Vec<DiffSignature>,
    /// Signatures present in A and gone in B: fixed, or simply not exercised.
    pub resolved_signatures: Vec<DiffSignature>,
    pub changed_counts: Vec<DiffSignature>,
    pub git: GitDiff,
    pub lockfiles_changed: Vec<LockfileChange>,
    pub env_added: Vec<String>,
    pub env_removed: Vec<String>,
    pub command_changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffSignature {
    pub fingerprint_id: FingerprintId,
    pub template: String,
    pub level: Level,
    pub count_a: i64,
    pub count_b: i64,
    pub sample: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitDiff {
    pub sha_a: Option<String>,
    pub sha_b: Option<String>,
    pub branch_a: Option<String>,
    pub branch_b: Option<String>,
    pub dirty_changed: bool,
    /// `git log --oneline a..b`, best effort; empty when the repo is gone or shallow.
    pub commits: Vec<GitCommit>,
    pub files_changed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommit {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockfileChange {
    pub file: String,
    pub hash_a: Option<String>,
    pub hash_b: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionState {
    pub project_id: ProjectId,
    pub bytes_used: i64,
    pub bytes_cap: i64,
    pub runs_total: i64,
    pub runs_evicted: i64,
    pub oldest_run_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Frame {
    Json(Vec<u8>),
    Data { captured_at: i64, bytes: Vec<u8> },
}

impl Frame {
    pub fn json<T: Serialize>(value: &T) -> crate::Result<Frame> {
        Ok(Frame::Json(serde_json::to_vec(value)?))
    }

    pub fn decode_json<T: for<'de> Deserialize<'de>>(&self) -> crate::Result<T> {
        match self {
            Frame::Json(bytes) => Ok(serde_json::from_slice(bytes)?),
            Frame::Data { .. } => Err(crate::Error::Protocol("expected a json frame".into())),
        }
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> std::io::Result<()> {
        match self {
            Frame::Json(payload) => {
                w.write_all(&[FRAME_JSON])?;
                w.write_all(&(payload.len() as u32).to_le_bytes())?;
                w.write_all(payload)?;
            }
            Frame::Data { captured_at, bytes } => {
                w.write_all(&[FRAME_DATA])?;
                w.write_all(&((bytes.len() + 8) as u32).to_le_bytes())?;
                w.write_all(&captured_at.to_le_bytes())?;
                w.write_all(bytes)?;
            }
        }
        Ok(())
    }

    /// Returns `Ok(None)` on a clean EOF at a frame boundary.
    pub fn read_from<R: Read>(r: &mut R) -> crate::Result<Option<Frame>> {
        let mut kind = [0u8; 1];
        match r.read_exact(&mut kind) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_FRAME {
            return Err(crate::Error::Protocol(format!(
                "frame of {len} bytes exceeds the {MAX_FRAME} byte limit"
            )));
        }
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload)?;
        match kind[0] {
            FRAME_JSON => Ok(Some(Frame::Json(payload))),
            FRAME_DATA => {
                if payload.len() < 8 {
                    return Err(crate::Error::Protocol("short data frame".into()));
                }
                let captured_at = i64::from_le_bytes(payload[..8].try_into().unwrap());
                Ok(Some(Frame::Data {
                    captured_at,
                    bytes: payload[8..].to_vec(),
                }))
            }
            other => Err(crate::Error::Protocol(format!("unknown frame kind {other}"))),
        }
    }
}

/// Metadata line at the head of a spool file, describing the run it holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpoolHeader {
    pub version: u32,
    pub stamp: RunStamp,
    pub started_at: i64,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub extra: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        Frame::json(&Request::Ping).unwrap().write_to(&mut buf).unwrap();
        Frame::Data {
            captured_at: 42,
            bytes: b"hello\r\n".to_vec(),
        }
        .write_to(&mut buf)
        .unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let f1 = Frame::read_from(&mut cursor).unwrap().unwrap();
        assert!(matches!(f1.decode_json::<Request>().unwrap(), Request::Ping));

        let f2 = Frame::read_from(&mut cursor).unwrap().unwrap();
        match f2 {
            Frame::Data { captured_at, bytes } => {
                assert_eq!(captured_at, 42);
                assert_eq!(bytes, b"hello\r\n");
            }
            _ => panic!("expected data frame"),
        }

        assert!(Frame::read_from(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn banner_is_silent_when_nothing_is_unread() {
        assert!(UnreadState::default().banner().is_none());
    }

    #[test]
    fn banner_counts_signatures() {
        let s = UnreadState {
            new_errors: 6,
            new_signatures: 2,
            oldest_at: Some(crate::now_micros() - 40_000_000),
            ..Default::default()
        };
        let b = s.banner().unwrap();
        assert!(b.contains("6 new errors"), "{b}");
        assert!(b.contains("2 new signatures"), "{b}");
        assert!(b.contains("40s ago"), "{b}");
    }
}

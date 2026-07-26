//! The shared vocabulary. Every binary and the frontend speak in these types.

use serde::{Deserialize, Serialize};

pub type RunId = i64;
pub type ProjectId = i64;
pub type EntryId = i64;
pub type FingerprintId = i64;

/// Canonical emulation width. The PTY handed to the child is sized to the user's real
/// terminal, but the *stored projection* is always emulated at this width so a wrap point
/// on a laptop and a wrap point on a 34" monitor cannot fingerprint differently.
pub const CANONICAL_COLS: usize = 200;

/// A single raw line may not grow past this without an LF before we force a synthetic
/// break. Protects against a progress bar that emits megabytes of `\r` and never a `\n`.
pub const MAX_RAW_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    Unknown,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
            Level::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Level {
        match s.to_ascii_lowercase().as_str() {
            "error" | "err" | "fatal" | "critical" | "crit" | "alert" | "emerg" => Level::Error,
            "warn" | "warning" => Level::Warn,
            "info" | "notice" => Level::Info,
            "debug" => Level::Debug,
            "trace" | "verbose" => Level::Trace,
            _ => Level::Unknown,
        }
    }
}

/// How entry boundaries were determined for a run. Recorded per run and exposed to agents
/// so they know how far to trust the grouping rather than assuming it is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelimiterMode {
    /// One JSON object per line. Exact.
    Json,
    /// Each entry wrapped in `ESC[<n>m … ESC[0m` (Elixir's Logger does this). Exact.
    ColourBracket,
    /// Line-start timestamp and/or level token. Exact for rigid formats.
    Prefix,
    /// Indentation plus known continuation markers. Inferred.
    Fallback,
}

impl DelimiterMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DelimiterMode::Json => "json",
            DelimiterMode::ColourBracket => "colour_bracket",
            DelimiterMode::Prefix => "prefix",
            DelimiterMode::Fallback => "fallback",
        }
    }

    pub fn parse(s: &str) -> DelimiterMode {
        match s {
            "json" => DelimiterMode::Json,
            "colour_bracket" => DelimiterMode::ColourBracket,
            "prefix" => DelimiterMode::Prefix,
            _ => DelimiterMode::Fallback,
        }
    }

    pub fn boundaries(self) -> Boundaries {
        match self {
            DelimiterMode::Fallback => Boundaries::Inferred,
            _ => Boundaries::Exact,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Boundaries {
    Exact,
    Inferred,
}

impl Boundaries {
    pub fn as_str(self) -> &'static str {
        match self {
            Boundaries::Exact => "exact",
            Boundaries::Inferred => "inferred",
        }
    }
}

/// Everything a run is stamped with at spawn. The git fields are the entire reason
/// "it worked yesterday, what changed?" is answerable rather than merely searchable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunStamp {
    pub command: Vec<String>,
    pub cwd: String,
    pub project_root: String,
    pub project_name: String,
    pub git_sha: Option<String>,
    pub git_branch: Option<String>,
    /// Hash of the dirty working tree, so two runs at the same SHA with different
    /// uncommitted edits are distinguishable.
    pub git_dirty_hash: Option<String>,
    /// `{"mix.lock": "<blake3>", ...}` for every lockfile present at the root.
    pub lockfile_hashes: std::collections::BTreeMap<String, String>,
    /// Environment variable **names** only. Values never leave the process (PRD §8).
    pub env_keys: Vec<String>,
    pub term_cols: u16,
    pub term_rows: u16,
    pub os: String,
    pub arch: String,
    pub shell: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub project_id: ProjectId,
    pub command: Vec<String>,
    pub cwd: String,
    pub session_group: Option<i64>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub git_sha: Option<String>,
    pub git_branch: Option<String>,
    pub git_dirty_hash: Option<String>,
    pub lockfile_hashes: std::collections::BTreeMap<String, String>,
    pub env_keys: Vec<String>,
    pub term_cols: u16,
    pub delimiter_mode: DelimiterMode,
    pub chunks_evicted: bool,
    pub counters: Counters,
}

impl Run {
    /// The definition of "it worked": exited cleanly and produced no error-level entry.
    pub fn is_clean(&self) -> bool {
        self.exit_code == Some(0) && self.counters.n_error == 0
    }

    pub fn command_line(&self) -> String {
        self.command.join(" ")
    }

    pub fn duration_micros(&self) -> Option<i64> {
        self.ended_at.map(|e| e - self.started_at)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Counters {
    pub n_entries: i64,
    pub n_error: i64,
    pub n_warn: i64,
    pub n_info: i64,
    pub n_debug: i64,
    pub n_trace: i64,
    pub n_unknown: i64,
}

impl Counters {
    pub fn bump(&mut self, level: Level) {
        self.n_entries += 1;
        match level {
            Level::Error => self.n_error += 1,
            Level::Warn => self.n_warn += 1,
            Level::Info => self.n_info += 1,
            Level::Debug => self.n_debug += 1,
            Level::Trace => self.n_trace += 1,
            Level::Unknown => self.n_unknown += 1,
        }
    }
}

/// A grouped log entry. Note that it carries **no text**: text lives once, in `chunks`,
/// and is produced on demand by re-rendering the line range. That is what lets the whole
/// entry table be dropped and rebuilt without touching truth or duplicating a byte.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub run_id: RunId,
    /// Inclusive index of the first raw line of this entry, within the run.
    pub start_line: i64,
    /// Inclusive index of the last raw line.
    pub end_line: i64,
    pub captured_at: i64,
    pub level: Level,
    pub is_error: bool,
    /// Which detection signals fired, comma separated. Empty when `is_error` is false.
    pub error_reason: Option<String>,
    pub fingerprint_id: Option<FingerprintId>,
    pub boundaries: Boundaries,
}

/// An entry plus its rendered text, which is what every consumer actually wants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryView {
    #[serde(flatten)]
    pub entry: Entry,
    pub text: String,
    pub template: Option<String>,
    /// True the first time this fingerprint has ever been seen in this project.
    pub is_new_signature: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fingerprint {
    pub id: FingerprintId,
    pub template: String,
    pub hash: String,
    pub level: Level,
    pub first_seen: i64,
    pub last_seen: i64,
    pub total_count: i64,
    /// Number of distinct runs this signature has ever appeared in.
    pub run_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub root: String,
    pub db_path: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    /// Retention cap in bytes. `None` means inherit the global default.
    pub retention_bytes: Option<i64>,
    pub push_enabled: bool,
    pub notify_enabled: bool,
}

/// A live snapshot of what the daemon is doing, for the home screen and `runlog status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectStatus {
    pub project: Project,
    pub active_runs: Vec<Run>,
    pub last_run: Option<Run>,
    pub db_bytes: i64,
    pub errors_last_hour: i64,
    pub unacked_errors: i64,
}

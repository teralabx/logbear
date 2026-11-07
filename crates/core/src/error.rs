use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Bare(#[from] std::io::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("no run with id {0}")]
    NoSuchRun(i64),

    #[error("no project registered for {0}")]
    NoSuchProject(String),

    /// A `logbear_query` call the sandbox refused. The message names the offending
    /// construct so an agent can repair its own SQL.
    #[error("query rejected: {0}")]
    QueryRejected(String),

    #[error("query exceeded its {0}ms budget and was cancelled")]
    QueryTimeout(u64),

    #[error("the logbear daemon is not running (expected socket at {0})")]
    DaemonUnreachable(PathBuf),

    #[error("protocol error: {0}")]
    Protocol(String),

    /// A check found damage that SQLite has not reported as an error of its own.
    #[error("{0} is damaged: {1}")]
    Corrupt(PathBuf, String),

    /// The file was written by a newer logbear. It is left exactly as it was: dropping what
    /// this build does not understand would destroy the user's history.
    #[error(
        "{path} is at storage version {found}, but this logbear understands up to {supported}. \
         Upgrade logbear to read it; the file has not been touched"
    )]
    DatabaseTooNew {
        path: PathBuf,
        found: i64,
        supported: i64,
    },

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    /// True when SQLite says the file itself is damaged, which retrying cannot fix.
    pub fn is_corrupt(&self) -> bool {
        matches!(self, Error::Corrupt(..))
            || matches!(
                self.sqlite_code(),
                Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
            )
    }

    /// True when a write failed for want of space. Transient: it clears when space does.
    pub fn is_disk_full(&self) -> bool {
        matches!(self.sqlite_code(), Some(rusqlite::ErrorCode::DiskFull))
    }

    fn sqlite_code(&self) -> Option<rusqlite::ErrorCode> {
        match self {
            Error::Sqlite(e) => e.sqlite_error_code(),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Other(e.to_string())
    }
}

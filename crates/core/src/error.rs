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

    /// A `runlog_query` call the sandbox refused. The message names the offending
    /// construct so an agent can repair its own SQL.
    #[error("query rejected: {0}")]
    QueryRejected(String),

    #[error("query exceeded its {0}ms budget and was cancelled")]
    QueryTimeout(u64),

    #[error("the runlog daemon is not running (expected socket at {0})")]
    DaemonUnreachable(PathBuf),

    #[error("protocol error: {0}")]
    Protocol(String),

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
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Other(e.to_string())
    }
}

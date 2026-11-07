//! Capture-stream parsing, the SQLite store, project resolution, diffing, redaction and
//! notification policy. The binaries (`logbear`, `logbeard`, `logbear-mcp`, `logbear-hook`)
//! and the Tauri backend are thin adapters over this crate.
//!
//! Two invariants constrain everything here:
//!
//! * C3 — raw bytes are truth. The `chunks` table is append-only and never rewritten;
//!   entries, levels, fingerprints, rendered text and the FTS index are projections that
//!   can be dropped and rebuilt from it.
//! * C1 — opacity. Nothing reaches into the wrapped process; its byte stream is the
//!   entire input surface.

pub mod client;
pub mod denylist;
pub mod diff;
pub mod error;
pub mod fence;
pub mod model;
pub mod notify;
pub mod parse;
pub mod paths;
pub mod project;
pub mod proto;
pub mod redact;
pub mod store;

pub use error::{Error, Result};
pub use model::*;

/// Current wall clock time in unix microseconds, the time base used throughout.
pub fn now_micros() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

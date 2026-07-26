//! runlog core.
//!
//! Everything runlog knows how to do lives here: capture-stream parsing, the SQLite
//! store, project resolution, diffing, redaction and the notification policy. The four
//! binaries (`runlog`, `runlogd`, `runlog-mcp`, `runlog-hook`) and the Tauri backend are
//! thin adapters over this crate and hold no logic of their own.
//!
//! Two invariants shape the whole design:
//!
//! * **C3 — raw bytes are truth.** [`store::chunks`] is append-only and never rewritten.
//!   Every derived thing (entries, levels, fingerprints, rendered text, the FTS index)
//!   is a projection that can be thrown away and rebuilt from the chunks.
//! * **C1 — total opacity.** Nothing in here reaches into the wrapped process. We read a
//!   byte stream and that is the entire input surface.

pub mod client;
pub mod diff;
pub mod error;
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

/// Current wall clock in unix microseconds — the single time base for the whole system.
pub fn now_micros() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

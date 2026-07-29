//! logbear's MCP server.
//!
//! Stdio transport, newline-delimited JSON-RPC 2.0, hand-rolled (see [`jsonrpc`]). It is a
//! thin client of the daemon: it owns no data, and the only state it keeps is the session
//! identity whose watermark lives in the daemon.
//!
//! The library target exists so the protocol can be driven from tests with a stub backend.

pub mod backend;
pub mod jsonrpc;
pub mod server;
pub mod tools;

pub use backend::{Backend, DaemonBackend};
pub use server::{derive_session_id, Server};

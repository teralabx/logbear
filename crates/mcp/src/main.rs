//! `logbear-mcp` — stdio MCP server.
//!
//! Read a line, answer it, flush. stdout carries protocol frames and nothing else: a stray
//! byte desynchronises the client's parser, so every diagnostic goes to stderr.

use logbear_mcp::{derive_session_id, DaemonBackend, Server};
use std::io::{BufRead, Write};

fn main() {
    let mut args = std::env::args().skip(1);
    if let Some(flag) = args.next() {
        match flag.as_str() {
            "--version" | "-V" => {
                println!("logbear-mcp {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                println!(
                    "logbear-mcp {}\n\nAn MCP (Model Context Protocol) server over stdio.\n\
                     It is started by your coding agent, not by hand.\n",
                    env!("CARGO_PKG_VERSION")
                );
                return;
            }
            other => {
                eprintln!("logbear-mcp: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }

    let session_id = derive_session_id();
    eprintln!("logbear-mcp {} session {session_id}", env!("CARGO_PKG_VERSION"));
    let mut server = Server::new(DaemonBackend::new(), session_id);

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("logbear-mcp: stdin read failed: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        for message in server.handle_line(&line) {
            // A write failure means the client is gone.
            if serde_json::to_writer(&mut stdout, &message)
                .map_err(|e| e.to_string())
                .and_then(|_| stdout.write_all(b"\n").map_err(|e| e.to_string()))
                .and_then(|_| stdout.flush().map_err(|e| e.to_string()))
                .is_err()
            {
                return;
            }
        }
    }
}

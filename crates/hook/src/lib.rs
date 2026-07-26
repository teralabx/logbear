//! runlog's harness hook — the thing that makes the product work.
//!
//! Everything else in runlog is a log tool. This binary is the part that *interrupts an
//! agent* the moment it claims to be done while the dev server is throwing exceptions.
//!
//! One binary serves both harnesses. Verified against current vendor documentation during
//! implementation, not assumed from the design spec:
//!
//! * **Claude Code** — `Stop` and `PostToolUse`, configured in `~/.claude/settings.json`.
//!   Blocks with `{"decision":"block","reason":…}`; injects with
//!   `hookSpecificOutput.additionalContext`. Payload carries `session_id`, `cwd`,
//!   `hook_event_name`, `stop_hook_active`.
//! * **Codex** — hooks are stable (`v0.124.0`) and on by default, configured in
//!   `~/.codex/hooks.json` or `[hooks]` in `config.toml`. `Stop`, `SubagentStop` and
//!   `PostToolUse` all exist with the same flat snake_case payload. Same block shape.
//!   `hookSpecificOutput` exists on `PostToolUse` but **not** on `Stop`.
//!
//! So this is one adapter with two config writers, exactly as §10.1 predicted.
//!
//! **Fail open, always.** Daemon down, malformed payload, timeout, unreadable field — every
//! one of them exits 0 with an allow response. A hook that breaks the user's agent because
//! our daemon is down is far worse than a hook that misses a notification, and this is the
//! only place in runlog where our code sits in the critical path of someone else's tool.

pub mod config;
pub mod decide;
pub mod payload;

use decide::Decision;
use payload::Payload;
use runlog_core::client::Client;
use runlog_core::model::EntryId;
use runlog_core::proto::{ProjectRef, UnreadState};
use runlog_core::Result;
use serde_json::{json, Value};
use std::time::Duration;

/// The daemon calls the hook needs, behind a trait so the loop-safety rules can be tested
/// without a running daemon — which matters here, because the interesting cases (budget
/// exhausted, ack refused) are ones a live daemon will not produce on demand.
pub trait Daemon {
    fn unread(&mut self, project: ProjectRef, session_id: &str, harness: &str)
        -> Result<UnreadState>;

    fn ack(
        &mut self,
        project: ProjectRef,
        session_id: &str,
        harness: &str,
        entry_id: Option<EntryId>,
        record_block: bool,
    ) -> Result<UnreadState>;
}

impl Daemon for Client {
    fn unread(
        &mut self,
        project: ProjectRef,
        session_id: &str,
        harness: &str,
    ) -> Result<UnreadState> {
        Client::unread(self, project, session_id, harness)
    }

    fn ack(
        &mut self,
        project: ProjectRef,
        session_id: &str,
        harness: &str,
        entry_id: Option<EntryId>,
        record_block: bool,
    ) -> Result<UnreadState> {
        Client::ack(self, project, session_id, harness, entry_id, record_block)
    }
}

/// The allow response: an empty object. Valid, minimal, and invisible.
///
/// It has to be exactly this. Codex treats non-empty unparseable stdout on `Stop` as a hard
/// hook failure, so "print nothing interesting" is not good enough — it must be JSON.
pub fn allow() -> Value {
    json!({})
}

/// Decide, act, and produce the JSON for stdout.
///
/// The ordering here is guard 3 of §10.3 and it is not cosmetic: **the cursor advances when
/// the block is issued**, not when the agent acknowledges. If the ack fails we downgrade to
/// advisory context rather than blocking, because a block whose watermark did not move
/// would re-fire on the same errors forever — the precise failure mode the guard exists to
/// prevent.
pub fn respond<D: Daemon>(payload: &Payload, daemon: &mut D) -> Value {
    let project = ProjectRef::Path(payload.cwd.clone());
    let harness = payload.harness.as_str();

    let unread = match daemon.unread(project.clone(), &payload.session_id, harness) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("runlog-hook: unread failed ({e}); allowing");
            return allow();
        }
    };

    let mut decision = decide::decide(&payload.event, &unread);

    if let Decision::Block { .. } = decision {
        let watermark = decide::watermark(&unread);
        if let Err(e) = daemon.ack(
            project,
            &payload.session_id,
            harness,
            watermark,
            /* record_block */ true,
        ) {
            eprintln!("runlog-hook: could not spend the block budget ({e}); degrading");
            decision = Decision::Context {
                text: match &decision {
                    Decision::Block { reason } => reason.clone(),
                    _ => unreachable!("checked immediately above"),
                },
            };
        }
    }

    decide::render(payload.harness, &payload.event, &decision)
}

/// Connect for a single hook invocation.
///
/// `connect`, never `connect_or_start`: a hook must not spawn a daemon and must not spend
/// seconds polling for one to come up while the user's agent waits. If runlogd is not
/// already running there is nothing to report anyway.
pub fn connect() -> Option<Client> {
    let mut client = Client::connect().ok()?;
    // A wedged daemon must not become a wedged agent.
    let _ = client.set_timeout(Some(Duration::from_secs(5)));
    Some(client)
}

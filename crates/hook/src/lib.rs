//! Harness hook for Claude Code and Codex.
//!
//! One binary serves both harnesses:
//!
//! * Claude Code — `Stop` and `PostToolUse`, configured in `~/.claude/settings.json`.
//! * Codex — `Stop`, `SubagentStop` and `PostToolUse`, configured in `~/.codex/hooks.json`
//!   or `[hooks]` in `config.toml`.
//!
//! Both deliver a flat snake_case payload carrying `session_id`, `cwd`, `hook_event_name`
//! and `stop_hook_active`, and both block with `{"decision":"block","reason":…}`.
//! `hookSpecificOutput` exists on Codex `PostToolUse` but not on Codex `Stop`.
//!
//! Every failure path fails open: daemon down, malformed payload, timeout or unreadable
//! field all exit 0 with an allow response.

pub mod config;
pub mod decide;
pub mod intent;
pub mod payload;

use decide::Decision;
use payload::Payload;
use logbear_core::client::Client;
use logbear_core::model::EntryId;
use logbear_core::proto::{AckKind, ProjectRef, UnreadState};
use logbear_core::Result;
use serde_json::{json, Value};
use std::time::Duration;

/// The daemon calls the hook needs.
///
/// A trait so the loop-safety rules can be tested without a running daemon: the interesting
/// cases (budget exhausted, ack refused) are ones a live daemon will not produce on demand.
pub trait Daemon {
    fn unread(&mut self, project: ProjectRef, session_id: &str, harness: &str)
        -> Result<UnreadState>;

    fn ack(
        &mut self,
        project: ProjectRef,
        session_id: &str,
        harness: &str,
        entry_id: Option<EntryId>,
        kind: AckKind,
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
        kind: AckKind,
    ) -> Result<UnreadState> {
        Client::ack(self, project, session_id, harness, entry_id, kind)
    }
}

/// The allow response: an empty JSON object.
///
/// It must be JSON: Codex treats non-empty unparseable stdout on `Stop` as a hard hook
/// failure.
pub fn allow() -> Value {
    json!({})
}

/// Decide, act, and produce the JSON for stdout.
///
/// The cursor advances when the block is issued, not when the agent acknowledges. A failed
/// ack downgrades the block to advisory context, because a block whose watermark did not
/// move would re-fire on the same errors forever.
pub fn respond<D: Daemon>(payload: &Payload, daemon: &mut D) -> Value {
    let project = ProjectRef::Path(payload.cwd.clone());
    let harness = payload.harness.as_str();

    let unread = match daemon.unread(project.clone(), &payload.session_id, harness) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("logbear-hook: unread failed ({e}); allowing");
            return allow();
        }
    };

    let mut decision = decide::decide(
        &decide::Input {
            event: &payload.event,
            tool: payload.tool_name.as_deref(),
            command: payload.command.as_deref(),
        },
        &unread,
    );

    match &decision {
        Decision::Block { budget, .. } => {
            let kind = match budget {
                decide::Budget::Stop => AckKind::StopBlock,
                decide::Budget::Change => AckKind::ChangeBlock,
            };
            let watermark = decide::watermark(&unread);
            if let Err(e) = daemon.ack(project, &payload.session_id, harness, watermark, kind) {
                eprintln!("logbear-hook: could not spend the block budget ({e}); degrading");
                decision = Decision::Context {
                    text: match &decision {
                        Decision::Block { reason, .. } => reason.clone(),
                        _ => unreachable!("checked immediately above"),
                    },
                    ack_nudge: false,
                };
            }
        }
        // A nudge that does not move its cursor fires again on every later change, so the
        // agent learns to ignore it.
        Decision::Context {
            ack_nudge: true, ..
        } => {
            if let Err(e) = daemon.ack(project, &payload.session_id, harness, None, AckKind::Nudge)
            {
                // Advisory only: the agent still gets told, it may just get told twice.
                eprintln!("logbear-hook: could not advance the nudge cursor ({e})");
            }
        }
        _ => {}
    }

    decide::render(payload.harness, &payload.event, &decision)
}

/// Connect for a single hook invocation.
///
/// `connect`, never `connect_or_start`: a hook must not spawn a daemon or poll for one to
/// come up while the agent waits.
pub fn connect() -> Option<Client> {
    let mut client = Client::connect().ok()?;
    // A wedged daemon must not become a wedged agent.
    let _ = client.set_timeout(Some(Duration::from_secs(5)));
    Some(client)
}

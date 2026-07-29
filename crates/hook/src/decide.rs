//! What to do about a session's unread errors, and how to say it in each dialect.
//!
//! [`decide`] is a pure function of the event and the daemon's [`UnreadState`]; [`render`]
//! is a pure function of the decision and the harness. Both are kept free of I/O so the
//! loop-safety rules are testable without a socket.

use crate::payload::{Event, Harness};
use logbear_core::model::EntryView;
use logbear_core::proto::UnreadState;
use serde_json::{json, Value};

/// How many entries are injected verbatim.
const MAX_ENTRIES: usize = 5;
/// Per-entry character cap. A single Elixir stack trace can run to several KB.
const MAX_ENTRY_CHARS: usize = 1_200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Say nothing. The common case.
    Allow,
    /// Stop events only: the agent is forced to continue with `reason` in front of it.
    Block { reason: String },
    /// Advisory text. Never interrupts the turn.
    Context { text: String },
}

pub fn decide(event: &Event, unread: &UnreadState) -> Decision {
    // Push is off for this project; nothing else applies.
    if !unread.push_enabled {
        return Decision::Allow;
    }
    if unread.is_empty() {
        return Decision::Allow;
    }

    match event {
        // Past the block budget, errors degrade to advisory context: blocking without a cap
        // traps the agent forever.
        Event::Stop | Event::SubagentStop if unread.blocks_remaining > 0 => Decision::Block {
            reason: block_reason(unread),
        },
        Event::Stop | Event::SubagentStop => Decision::Context {
            text: summary(unread),
        },
        Event::PostToolUse => Decision::Context {
            text: summary(unread),
        },
        Event::Other(_) => Decision::Allow,
    }
}

/// The JSON a harness expects on stdout.
///
/// Both dialects agree on `{"decision":"block","reason":...}` and on
/// `hookSpecificOutput.additionalContext` for advisory text, and diverge in one place:
///
/// * Codex's `Stop` output schema is `additionalProperties: false` and defines no
///   `hookSpecificOutput`; emitting `additionalContext` there fails the whole hook. Its
///   only non-blocking key is `systemMessage`, which Codex renders as a UI warning and
///   never puts in front of the model, so a degraded stop reaches the human and not the
///   agent.
/// * Codex's `PostToolUse` does carry `hookSpecificOutput`, with `hookEventName` in
///   camelCase and required even though the input field is snake_case.
///
/// On Codex `Stop`, non-empty stdout that does not parse as JSON is a hard hook failure.
pub fn render(harness: Harness, event: &Event, decision: &Decision) -> Value {
    match decision {
        Decision::Allow => json!({}),
        Decision::Block { reason } => json!({ "decision": "block", "reason": reason }),
        Decision::Context { text } => {
            if harness == Harness::Codex && event.is_stop() {
                json!({ "systemMessage": text })
            } else {
                json!({
                    "hookSpecificOutput": {
                        "hookEventName": event.name(),
                        "additionalContext": text,
                    }
                })
            }
        }
    }
}

/// The blocking form: the entries, their templates, whether each is new, and the run.
fn block_reason(u: &UnreadState) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "logbear: the process you are working on logged {} new error{}{} since you last looked. \
         Do not report this task as finished until you have accounted for {}.\n",
        u.new_errors,
        plural(u.new_errors),
        signatures(u.new_signatures),
        if u.new_errors == 1 { "it" } else { "them" },
    ));

    for (i, e) in u.entries.iter().take(MAX_ENTRIES).enumerate() {
        out.push('\n');
        out.push_str(&format!(
            "[{}] {} · run {} · {} · {}\n",
            i + 1,
            // `is_new_signature` is scoped to the project; the header count is scoped to
            // this session. Both are spelled out so the two cannot be read as contradictory.
            if e.is_new_signature {
                "NEW SIGNATURE — never seen in this project before"
            } else {
                "recurring — this signature appeared in an earlier run"
            },
            e.entry.run_id,
            ago(e.entry.captured_at),
            e.entry.level.as_str(),
        ));
        if let Some(t) = &e.template {
            out.push_str(&format!("    signature: {}\n", one_line(t)));
        }
        for line in truncate(&e.text, MAX_ENTRY_CHARS).lines() {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
        if e.entry.boundaries == logbear_core::model::Boundaries::Inferred {
            out.push_str("    (entry boundaries inferred — this may be under- or over-grouped)\n");
        }
    }

    let shown = u.entries.len().min(MAX_ENTRIES);
    if u.entries.len() > shown {
        out.push_str(&format!("\n… and {} more.\n", u.entries.len() - shown));
    }
    out.push_str(
        "\nUse the logbear MCP tools to investigate, or run `logbear logs --level error`. \
         If these errors are expected, say so and stop.\n",
    );
    out
}

/// The advisory form: one compact line.
///
/// Never acknowledged, so it may repeat across tool calls. Acknowledging here would advance
/// the cursor and rob the Stop hook of the errors it exists to surface.
fn summary(u: &UnreadState) -> String {
    let newest = u
        .entries
        .last()
        .map(|e| {
            let head = e
                .template
                .clone()
                .unwrap_or_else(|| e.text.lines().next().unwrap_or_default().to_string());
            format!(" Most recent: {}", one_line(&truncate(&head, 200)))
        })
        .unwrap_or_default();
    format!(
        "logbear: {} new error{}{} in this project{}.{} Check with the logbear MCP tools or \
         `logbear logs --level error`.",
        u.new_errors,
        plural(u.new_errors),
        signatures(u.new_signatures),
        u.oldest_at
            .map(|t| format!(", oldest {}", ago(t)))
            .unwrap_or_default(),
        newest,
    )
}

/// The entry the cursor advances to when a block is issued.
pub fn watermark(u: &UnreadState) -> Option<i64> {
    u.newest_entry_id
        .or_else(|| u.entries.iter().map(|e: &EntryView| e.entry.id).max())
}

fn signatures(n: i64) -> String {
    if n > 0 {
        format!(" ({n} distinct signature{})", plural(n))
    } else {
        String::new()
    }
}

fn plural(n: i64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn ago(at: i64) -> String {
    let secs = ((logbear_core::now_micros() - at) / 1_000_000).max(0);
    if secs < 90 {
        format!("{secs}s ago")
    } else if secs < 5400 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n… (truncated)")
}

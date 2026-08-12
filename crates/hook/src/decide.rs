//! What to do about a session's unread errors, and how to say it in each dialect.
//!
//! [`decide`] is a pure function of the event and the daemon's [`UnreadState`]; [`render`]
//! is a pure function of the decision and the harness. Both are kept free of I/O so the
//! loop-safety rules are testable without a socket.

use crate::intent::{self, Intent};
use crate::payload::{Event, Harness};
use logbear_core::model::EntryView;
use logbear_core::proto::UnreadState;
use serde_json::{json, Value};

/// How many entries are injected verbatim.
const MAX_ENTRIES: usize = 5;
/// Per-entry character cap. A single Elixir stack trace can run to several KB.
const MAX_ENTRY_CHARS: usize = 1_200;

/// Which allowance a block spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// The Stop gate: the last thing standing between a broken change and "done".
    Stop,
    /// A mid-turn interruption, on its own smaller allowance.
    Change,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Say nothing. The common case, and the one most rules end at.
    Allow,
    /// The agent is forced to continue with `reason` in front of it.
    Block { reason: String, budget: Budget },
    /// Advisory text. Never interrupts the turn.
    ///
    /// `ack_nudge` advances the nudge cursor. Set only by the verify nudge: every other
    /// advisory reports errors the Stop hook still needs to see.
    Context { text: String, ack_nudge: bool },
}

/// Everything the rules read, so the signature does not grow a parameter per rule.
pub struct Input<'a> {
    pub event: &'a Event,
    pub tool: Option<&'a str>,
    pub command: Option<&'a str>,
    /// The harness's own re-entrancy flag: a hook already blocked the preceding stop.
    pub stop_hook_active: bool,
}

impl Input<'_> {
    /// Tools that change code. Both harnesses' names, since one binary serves both.
    fn is_change(&self) -> bool {
        matches!(
            self.tool,
            Some("Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "apply_patch" | "edit_file")
        )
    }
}

pub fn decide(input: &Input, unread: &UnreadState) -> Decision {
    // Push is off for this project; nothing else applies.
    if !unread.push_enabled {
        return Decision::Allow;
    }

    match input.event {
        // Said once, up front, so the agent gets it right the first time rather than being
        // corrected after the first server's output is already lost.
        Event::SessionStart => Decision::Context {
            text: STANDING_RULE.to_string(),
            ack_nudge: false,
        },

        Event::Stop | Event::SubagentStop if unread.is_empty() => Decision::Allow,
        // The harness says a hook already blocked the stop this turn, so the agent has been
        // told once and blocking again cannot teach it anything new. This outranks the block
        // budget on purpose: the budget lives in the session cursor, so anything that stops
        // that cursor persisting — a failed ack, a session id that changes between
        // invocations — spends an allowance that never depletes and the agent is trapped
        // until the harness's own consecutive-block cap saves it. This flag arrives on every
        // payload and needs nothing to have been written down.
        Event::Stop | Event::SubagentStop if input.stop_hook_active => Decision::Context {
            text: summary(unread),
            ack_nudge: false,
        },
        // Every unread signature has already been surfaced to this session — by an
        // earlier block or a read the daemon verified. Re-blocking on them cannot teach
        // the agent anything new; only an unseen signature is worth a turn.
        Event::Stop | Event::SubagentStop if unread.new_signatures == 0 => Decision::Context {
            text: recurring_summary(unread),
            ack_nudge: false,
        },
        // Past the block budget, errors degrade to advisory context: blocking without a cap
        // traps the agent forever.
        Event::Stop | Event::SubagentStop if unread.blocks_remaining > 0 => Decision::Block {
            reason: block_reason(unread),
            budget: Budget::Stop,
        },
        Event::Stop | Event::SubagentStop => Decision::Context {
            text: summary(unread),
            ack_nudge: false,
        },

        Event::PostToolUse => post_tool_use(input, unread),
        Event::Other(_) => Decision::Allow,
    }
}

/// One message per event, and an error is always the more useful one.
fn post_tool_use(input: &Input, unread: &UnreadState) -> Decision {
    if !unread.is_empty() {
        // A signature this project has never produced, arriving right after a change, is
        // almost certainly that change — and is cheapest to fix while the agent still has
        // the context for it. A recurring one is not: it was already there.
        if input.is_change() && unread.new_signatures > 0 && unread.change_blocks_remaining > 0 {
            return Decision::Block {
                reason: block_reason(unread),
                budget: Budget::Change,
            };
        }
        return Decision::Context {
            text: summary(unread),
            ack_nudge: false,
        };
    }

    // Nothing has gone wrong that logbear can see. The change may still be wrong, and the
    // running process is the only thing that knows.
    if input.is_change() && unread.server_running && unread.unseen_lines > 0 {
        return Decision::Context {
            text: verify_nudge(unread.unseen_lines),
            ack_nudge: true,
        };
    }

    if let Some(command) = input.command {
        return match intent::classify(command) {
            Intent::StartsServer { suggested } => Decision::Context {
                text: format!(
                    "logbear: that process is running uncaptured, so nothing is recording its \
                     output and its errors will not reach you. Stop it and start it again as \
                     `{suggested}`.",
                ),
                ack_nudge: false,
            },
            // Only worth saying when there is something to redirect to; on a project with
            // nothing captured the advice points at an empty database.
            Intent::ChasesBug if unread.server_running => Decision::Context {
                text: "logbear: this project's output is already captured. \
                       `logbear logs --level error`, or the logbear MCP tools, cover the whole \
                       run rather than whatever happened to reach that file."
                    .to_string(),
                ack_nudge: false,
            },
            _ => Decision::Allow,
        };
    }

    Decision::Allow
}

/// What the agent needs to know before it starts anything.
///
/// Deliberately does not claim this project is already being captured: the rule fires in
/// every session, and the one where it matters most is a project logbear has never seen.
const STANDING_RULE: &str = "\
logbear captures long-running processes and surfaces their errors to you. Start dev servers, \
watchers — anything that runs until you kill it — with `logbear` in front, for example \
`logbear npm run dev` or `logbear mix phx.server`. Once one is running you can read its \
output with `logbear logs`. One-off commands like tests and builds do not need it, since you \
already see their output.";

/// The change landed and the process reacted. Whether it reacted *well* is the part the agent
/// cannot know without looking, and a clean compile is not an answer.
fn verify_nudge(lines: i64) -> String {
    format!(
        "logbear: the running process logged {lines} new line{} after your change. Read them \
         with `logbear logs` or the logbear MCP tools to confirm it did what you intended — \
         a change that compiles can still be wrong.",
        plural(lines),
    )
}

/// The JSON a harness expects on stdout.
///
/// Both dialects agree on `{"decision":"block","reason":...}` and on
/// `hookSpecificOutput.additionalContext` for advisory text on `PostToolUse` (Codex wants
/// `hookEventName` in camelCase and required even though the input field is snake_case).
///
/// Stop events have no model-visible, non-blocking channel in either dialect:
///
/// * Codex's `Stop` output schema is `additionalProperties: false` and defines no
///   `hookSpecificOutput`; emitting `additionalContext` there fails the whole hook, and
///   non-empty stdout that does not parse as JSON is a hard hook failure.
/// * Claude Code does not define `additionalContext` for `Stop`/`SubagentStop` either —
///   in practice it comes back as "Stop hook feedback" that re-opens the turn and counts
///   toward the consecutive-block cap. An advisory that keeps the agent running is a
///   block wearing different clothes.
///
/// So a stop-time advisory degrades to `systemMessage` everywhere: a UI warning that
/// reaches the human, not the agent, while the turn is allowed to end.
pub fn render(_harness: Harness, event: &Event, decision: &Decision) -> Value {
    match decision {
        Decision::Allow => json!({}),
        Decision::Block { reason, .. } => json!({ "decision": "block", "reason": reason }),
        Decision::Context { text, .. } => {
            if event.is_stop() {
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

/// The degraded stop: errors exist, but this session has seen every signature.
fn recurring_summary(u: &UnreadState) -> String {
    format!(
        "logbear: {} unread error{}, but every signature is one you have already seen \
         this session — nothing new to account for. Recheck with `logbear logs --level \
         error` if that seems wrong.",
        u.new_errors,
        plural(u.new_errors),
    )
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

    let mut entries: Vec<&EntryView> = u.entries.iter().collect();
    // The new signatures are why this block fired; MAX_ENTRIES must never truncate
    // them away in favour of recurring ones. Stable, so chronology holds within each group.
    entries.sort_by_key(|e| !e.is_new_signature);

    for (i, e) in entries.iter().take(MAX_ENTRIES).enumerate() {
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

    let shown = entries.len().min(MAX_ENTRIES);
    if entries.len() > shown {
        out.push_str(&format!("\n… and {} more.\n", entries.len() - shown));
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
        .and_then(|e| {
            let head = e
                .template
                .clone()
                .unwrap_or_else(|| e.text.lines().next().unwrap_or_default().to_string());
            let head = one_line(&truncate(&head, 200));
            let head = head.trim();
            // An entry whose template and first line are both blank would render as
            // "Most recent: " followed by nothing, which reads like a bug.
            (!head.is_empty()).then(|| format!(" Most recent: {head}."))
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

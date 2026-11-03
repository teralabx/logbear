//! Normalising each harness's stdin payload into one shape.
//!
//! Claude Code and Codex deliver a flat, snake_case JSON object carrying `session_id`,
//! `cwd`, `hook_event_name` and `stop_hook_active`, and the pi extension builds the same
//! object, so those are one parser with a harness tag. Hermes shares the envelope but names
//! its events differently and keeps event details under `extra`; [`hermes_event`] maps them.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    ClaudeCode,
    Codex,
    /// pi has no command hooks; logbear's extension builds a Claude-shaped payload and reads
    /// a Claude-shaped reply.
    Pi,
    Hermes,
}

impl Harness {
    /// The value written to `session_cursors.harness`.
    pub fn as_str(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude_code",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::Hermes => "hermes",
        }
    }

    /// Whether a stop-time advisory can reach anyone without re-opening the turn.
    ///
    /// Hermes reads any message on `pre_verify` as "keep going", so there it has nowhere to
    /// go and the errors must stay unread for the next turn's context to carry.
    pub fn has_stop_advisory(self) -> bool {
        !matches!(self, Harness::Hermes)
    }

    pub fn parse(s: &str) -> Option<Harness> {
        match s {
            "claude" | "claude_code" | "claude-code" => Some(Harness::ClaudeCode),
            "codex" => Some(Harness::Codex),
            "pi" => Some(Harness::Pi),
            "hermes" => Some(Harness::Hermes),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Stop,
    SubagentStop,
    PostToolUse,
    SessionStart,
    /// A lifecycle event logbear does not register for. Always allowed.
    Other(String),
}

impl Event {
    pub fn parse(s: &str) -> Event {
        match s {
            "Stop" => Event::Stop,
            "SubagentStop" => Event::SubagentStop,
            "PostToolUse" => Event::PostToolUse,
            "SessionStart" => Event::SessionStart,
            other => Event::Other(other.to_string()),
        }
    }

    /// The value echoed back in `hookSpecificOutput.hookEventName`; both harnesses require
    /// it to match the event that fired.
    pub fn name(&self) -> &str {
        match self {
            Event::Stop => "Stop",
            Event::SubagentStop => "SubagentStop",
            Event::PostToolUse => "PostToolUse",
            Event::SessionStart => "SessionStart",
            Event::Other(s) => s,
        }
    }

    pub fn is_stop(&self) -> bool {
        matches!(self, Event::Stop | Event::SubagentStop)
    }
}

#[derive(Debug, Clone)]
pub struct Payload {
    pub harness: Harness,
    pub event: Event,
    pub session_id: String,
    pub cwd: String,
    /// True when a hook already blocked the preceding stop. Overrides the block budget: it
    /// is the one loop guard that does not depend on the session cursor having persisted, so
    /// it holds even when the budget silently fails to deplete. Both harnesses also cap
    /// consecutive blocks themselves (Claude Code at eight), as a last resort beneath this.
    pub stop_hook_active: bool,
    pub tool_name: Option<String>,
    /// The shell command, for shell-running tools only.
    ///
    /// Read from `tool_input.command` for every tool but Codex's `apply_patch`, which puts
    /// the patch body in the same field. Shell tools have had several names across harness
    /// versions, so excluding the one non-shell user is safer than listing the shells.
    pub command: Option<String>,
}

/// Parse a harness payload. `forced` comes from `--harness` and always wins.
///
/// `None` means the payload was unusable; the caller must then allow.
pub fn parse(input: &str, forced: Option<Harness>) -> Option<Payload> {
    let value: Value = serde_json::from_str(input).ok()?;
    let obj = value.as_object()?;

    let session_id = obj.get("session_id")?.as_str()?.to_string();
    if session_id.is_empty() {
        return None;
    }
    // Both harnesses document `cwd` as required, but the hook is spawned with the session's
    // working directory anyway, so the process cwd is a safe fallback.
    let cwd = obj
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })?;

    let tool_name = obj.get("tool_name").and_then(|v| v.as_str());
    let harness = forced.unwrap_or_else(|| detect_harness(obj));
    let event_name = obj.get("hook_event_name").and_then(|v| v.as_str()).unwrap_or("");
    let (event, stop_hook_active) = match harness {
        Harness::Hermes => hermes_event(event_name, obj.get("extra")),
        _ => (
            Event::parse(event_name),
            obj.get("stop_hook_active").and_then(|v| v.as_bool()).unwrap_or(false),
        ),
    };

    Some(Payload {
        harness,
        event,
        session_id,
        cwd,
        stop_hook_active,
        tool_name: tool_name.map(str::to_string),
        command: (tool_name != Some("apply_patch"))
            .then(|| obj.get("tool_input").and_then(|v| command_of(v.get("command")?)))
            .flatten(),
    })
}

/// The shell command a tool call ran, from either harness's spelling of it.
///
/// Both harnesses now send a string. Older Codex builds sent an argv array wrapping what the
/// model asked for in `["bash", "-lc", "…"]`, so for those the meaningful command is the
/// script argument, not the joined array, which would read as a `bash` invocation and match
/// nothing.
fn command_of(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    let argv: Vec<&str> = value.as_array()?.iter().filter_map(|v| v.as_str()).collect();
    match argv.as_slice() {
        [shell, flag, script]
            if matches!(*shell, "bash" | "sh" | "zsh" | "fish") && flag.starts_with('-') =>
        {
            Some(script.to_string())
        }
        [] => None,
        _ => Some(argv.join(" ")),
    }
}

/// Hermes's events, in the terms the rules are written in.
///
/// * `pre_llm_call` fires once per user turn. On the first it is the session start; on the
///   rest it is the one point a shell hook can add context, so it stands in for
///   `PostToolUse` with no tool — unread errors are told, nothing is blocked.
/// * `pre_verify` is the round-end gate, so it is the stop. It only fires on turns that
///   edited code, which is why later turns' `pre_llm_call` has to carry the rest.
///   `attempt` counts nudges already given this turn: Hermes's `stop_hook_active`.
fn hermes_event(name: &str, extra: Option<&Value>) -> (Event, bool) {
    let field = |key: &str| extra.and_then(|e| e.get(key));
    match name {
        "pre_llm_call" if field("is_first_turn").and_then(Value::as_bool) == Some(true) => {
            (Event::SessionStart, false)
        }
        "pre_llm_call" => (Event::PostToolUse, false),
        "pre_verify" => (
            Event::Stop,
            field("attempt").and_then(Value::as_i64).unwrap_or(0) > 0,
        ),
        other => (Event::Other(other.to_string()), false),
    }
}

/// Guess the harness from vendor-specific fields: `turn_id` is Codex-only, and Hermes is the
/// one that adds `profile` and nests the rest under `extra`.
///
/// A fallback for hand-written configs; every config logbear emits passes `--harness`.
fn detect_harness(obj: &serde_json::Map<String, Value>) -> Harness {
    if obj.contains_key("profile") && obj.get("extra").is_some_and(Value::is_object) {
        Harness::Hermes
    } else if obj.contains_key("turn_id") {
        Harness::Codex
    } else {
        Harness::ClaudeCode
    }
}

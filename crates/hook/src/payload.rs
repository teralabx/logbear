//! Normalising the two harnesses' stdin payloads into one shape.
//!
//! Both deliver a flat, snake_case JSON object carrying `session_id`, `cwd`,
//! `hook_event_name` and `stop_hook_active`, so this is one parser with a harness tag.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    ClaudeCode,
    Codex,
}

impl Harness {
    /// The value written to `session_cursors.harness`.
    pub fn as_str(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude_code",
            Harness::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Harness> {
        match s {
            "claude" | "claude_code" | "claude-code" => Some(Harness::ClaudeCode),
            "codex" => Some(Harness::Codex),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Stop,
    SubagentStop,
    PostToolUse,
    /// A lifecycle event runlog does not register for. Always allowed.
    Other(String),
}

impl Event {
    pub fn parse(s: &str) -> Event {
        match s {
            "Stop" => Event::Stop,
            "SubagentStop" => Event::SubagentStop,
            "PostToolUse" => Event::PostToolUse,
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
    /// True when a hook already blocked the preceding stop. Both harnesses cap consecutive
    /// blocks themselves (Claude Code at eight), underneath runlog's own block budget.
    pub stop_hook_active: bool,
    pub tool_name: Option<String>,
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

    Some(Payload {
        harness: forced.unwrap_or_else(|| detect_harness(obj)),
        event: Event::parse(obj.get("hook_event_name").and_then(|v| v.as_str()).unwrap_or("")),
        session_id,
        cwd,
        stop_hook_active: obj
            .get("stop_hook_active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        tool_name: obj
            .get("tool_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

/// Guess the harness from a vendor-specific extension field: `turn_id` is Codex-only.
///
/// A fallback for hand-written configs; every config runlog emits passes `--harness`.
fn detect_harness(obj: &serde_json::Map<String, Value>) -> Harness {
    if obj.contains_key("turn_id") {
        Harness::Codex
    } else {
        Harness::ClaudeCode
    }
}

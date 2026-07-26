//! Normalising the two harnesses' stdin payloads into one shape.
//!
//! Verified against current documentation during implementation (see the crate docs). The
//! happy discovery is that Codex modelled its hooks engine on Claude Code's deliberately:
//! both deliver a flat, snake_case JSON object carrying `session_id`, `cwd`,
//! `hook_event_name` and `stop_hook_active`. Those are exactly the fields runlog needs, so
//! this is one parser with a harness tag, not two integrations.

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
    /// Some other lifecycle event. Always allowed — runlog registers for three, and a
    /// payload for anything else means someone wired us up more broadly than we planned.
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

    /// The value echoed back in `hookSpecificOutput.hookEventName`, which both harnesses
    /// require to match the event that fired.
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
    /// blocks themselves (Claude Code at eight), which is a second net under runlog's own
    /// block budget rather than a replacement for it.
    pub stop_hook_active: bool,
    pub tool_name: Option<String>,
}

/// Parse a harness payload. `forced` comes from `--harness` and always wins.
///
/// `None` means the payload was unusable, and the only correct response to that is to allow
/// (§10.3): a hook that breaks the user's agent because we could not read a field is far
/// worse than a hook that misses one notification.
pub fn parse(input: &str, forced: Option<Harness>) -> Option<Payload> {
    let value: Value = serde_json::from_str(input).ok()?;
    let obj = value.as_object()?;

    let session_id = obj.get("session_id")?.as_str()?.to_string();
    if session_id.is_empty() {
        return None;
    }
    // cwd is documented as required by both harnesses, but falling back to the process cwd
    // is strictly better than dropping the notification: the hook is spawned with the
    // session's working directory anyway.
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

/// A fallback only. Every config runlog emits passes `--harness` explicitly, so this
/// matters for hand-written configs, and it keys on each vendor's own extension field:
/// `turn_id` is Codex-only, `prompt_id` is Claude-Code-only.
fn detect_harness(obj: &serde_json::Map<String, Value>) -> Harness {
    if obj.contains_key("turn_id") {
        Harness::Codex
    } else {
        Harness::ClaudeCode
    }
}

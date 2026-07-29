//! The config blocks that register this binary with each harness.
//!
//! One source of truth, shared by the installer and `logbear doctor`. Both fragments are
//! top-level objects containing only a `hooks` key, so they merge into an existing config
//! without disturbing it.

use crate::payload::Harness;
use serde_json::{json, Value};

/// The Claude Code tools worth firing `PostToolUse` on: those that change code or run
/// commands.
///
/// Claude Code compiles matchers as regexes, so this alternation behaves as written.
const CLAUDE_POST_TOOL_MATCHER: &str = "Edit|Write|MultiEdit|NotebookEdit|Bash";

/// Hook timeout, in seconds. Well below either harness's default, so a wedged daemon shows
/// up as a slow hook rather than a stalled agent.
const TIMEOUT_SECS: u64 = 10;

/// A note the installer and `logbear doctor` show alongside the Codex block.
///
/// Codex will not run a non-managed command hook until the user trusts it, and an untrusted
/// hook fails by silently never firing.
pub const CODEX_TRUST_NOTE: &str =
    "Codex will not run a non-managed command hook until you trust it. \
     Run `/hooks` inside Codex to review and trust the logbear hook, or start Codex with \
     --dangerously-bypass-hook-trust in automation.";

pub fn emit(harness: Harness, command: &str) -> Value {
    match harness {
        Harness::ClaudeCode => claude(command),
        Harness::Codex => codex(command),
    }
}

fn handler(command: &str, harness: Harness) -> Value {
    json!({
        "type": "command",
        "command": format!("{command} --harness {}", match harness {
            Harness::ClaudeCode => "claude",
            Harness::Codex => "codex",
        }),
        "timeout": TIMEOUT_SECS,
    })
}

fn claude(command: &str) -> Value {
    let h = handler(command, Harness::ClaudeCode);
    json!({
        "hooks": {
            // An empty matcher is Claude Code's "every invocation".
            "Stop": [{ "matcher": "", "hooks": [h] }],
            "SessionStart": [{ "matcher": "", "hooks": [h] }],
            "PostToolUse": [{ "matcher": CLAUDE_POST_TOOL_MATCHER, "hooks": [h] }],
        }
    })
}

fn codex(command: &str) -> Value {
    let h = handler(command, Harness::Codex);
    json!({
        "hooks": {
            // Codex ignores `matcher` on Stop entirely.
            "Stop": [{ "hooks": [h] }],
            "SubagentStop": [{ "hooks": [h] }],
            "SessionStart": [{ "hooks": [h] }],
            // No matcher on PostToolUse either: a Codex matcher made only of [A-Za-z0-9_|]
            // is exact alternation rather than a regex, so a guessed tool name would
            // produce a hook that never fires and never says why. Omitting it matches
            // every tool.
            "PostToolUse": [{ "hooks": [h] }],
        }
    })
}

/// Path to this binary, quoted for the shell.
///
/// Both harnesses run command hooks through a shell (`sh -c` / `$SHELL -lc`), so a path
/// containing a space has to survive word splitting.
pub fn self_command() -> String {
    let path = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "logbear-hook".into());
    shell_quote(&path)
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/=+:@".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_block_has_both_events_and_a_command() {
        let v = emit(Harness::ClaudeCode, "/usr/local/bin/logbear-hook");
        let stop = &v["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(stop["type"], "command");
        assert_eq!(stop["command"], "/usr/local/bin/logbear-hook --harness claude");
        assert_eq!(v["hooks"]["Stop"][0]["matcher"], "");
        assert!(v["hooks"]["PostToolUse"][0]["matcher"]
            .as_str()
            .unwrap()
            .contains("Bash"));
    }

    #[test]
    fn codex_block_carries_only_the_keys_codex_allows() {
        let v = emit(Harness::Codex, "/usr/local/bin/logbear-hook");
        // The Codex hooks file denies unknown fields at the top level.
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["hooks"]);
        for event in ["Stop", "SubagentStop", "SessionStart", "PostToolUse"] {
            let entry = &v["hooks"][event][0];
            assert!(
                entry.get("matcher").is_none(),
                "{event} must not carry a matcher"
            );
            let h = &entry["hooks"][0];
            assert_eq!(h["command"], "/usr/local/bin/logbear-hook --harness codex");
            // `timeout`, in seconds — not `timeoutSec`.
            assert_eq!(h["timeout"], 10);
            let handler_keys: Vec<&String> = h.as_object().unwrap().keys().collect();
            assert_eq!(handler_keys, vec!["command", "timeout", "type"]);
        }
    }

    /// The standing rule is only worth writing if it is actually delivered, and it is the one
    /// hook that has to fire before the agent does anything.
    #[test]
    fn both_harnesses_register_session_start() {
        for harness in [Harness::ClaudeCode, Harness::Codex] {
            let v = emit(harness, "/usr/local/bin/logbear-hook");
            assert!(
                v["hooks"]["SessionStart"][0]["hooks"][0]["command"].is_string(),
                "{harness:?} must register SessionStart"
            );
        }
    }

    #[test]
    fn paths_with_spaces_survive_the_shell() {
        assert_eq!(shell_quote("/usr/local/bin/x"), "/usr/local/bin/x");
        assert_eq!(
            shell_quote("/Applications/My App.app/hook"),
            "'/Applications/My App.app/hook'"
        );
    }
}

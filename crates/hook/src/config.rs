//! What registers this binary with each harness.
//!
//! One source of truth, shared by the installer and `logbear doctor`: `--emit-config`
//! prints [`render`]'s output. Claude Code and Codex take a JSON object containing only a
//! `hooks` key, so it merges into an existing config without disturbing it. Hermes takes the
//! same shape as a YAML `hooks:` block. pi has no hook config at all and takes an extension
//! source instead.

use crate::payload::Harness;
use serde_json::{json, Value};

/// The Claude Code tools worth firing `PostToolUse` on: those that change code or run
/// commands.
///
/// Made only of letters and `|`, so Claude Code reads it as a list of exact tool names
/// rather than a regex.
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

/// The Hermes events logbear registers. See `payload::hermes_event` for what each stands in
/// for.
const HERMES_EVENTS: [&str; 2] = ["pre_llm_call", "pre_verify"];

/// What `--emit-config` prints, given this binary's unquoted path.
pub fn render(harness: Harness, hook_path: &str) -> String {
    let command = shell_quote(hook_path);
    match harness {
        Harness::ClaudeCode | Harness::Codex | Harness::Hermes => {
            let block = emit(harness, &command).expect("a JSON-config harness");
            match harness {
                Harness::Hermes => hermes_yaml(&block),
                _ => serde_json::to_string_pretty(&block).expect("a literal json value serialises"),
            }
        }
        Harness::Pi => pi_extension(hook_path),
    }
}

/// The hooks block for a harness configured by one, with `command` already shell-quoted.
/// `None` for pi, which takes [`pi_extension`] instead.
/// Hermes asks once per (event, command) pair before running a shell hook, and an
/// unapproved hook is skipped rather than failing.
pub const HERMES_TRUST_NOTE: &str =
    "Hermes asks before running a new shell hook. Approve both logbear entries when it \
     prompts, or review them with `hermes hooks`.";

pub fn emit(harness: Harness, command: &str) -> Option<Value> {
    match harness {
        Harness::ClaudeCode => Some(claude(command)),
        Harness::Codex => Some(codex(command)),
        Harness::Hermes => Some(hermes(command)),
        Harness::Pi => None,
    }
}

fn harness_flag(command: &str, harness: Harness) -> String {
    let name = match harness {
        Harness::ClaudeCode => "claude",
        Harness::Codex => "codex",
        Harness::Pi => "pi",
        Harness::Hermes => "hermes",
    };
    format!("{command} --harness {name}")
}

fn handler(command: &str, harness: Harness) -> Value {
    json!({
        "type": "command",
        "command": harness_flag(command, harness),
        "timeout": TIMEOUT_SECS,
    })
}

fn claude(command: &str) -> Value {
    let h = handler(command, Harness::ClaudeCode);
    json!({
        "hooks": {
            // An empty matcher is Claude Code's "every invocation".
            "Stop": [{ "matcher": "", "hooks": [h] }],
            "SubagentStop": [{ "matcher": "", "hooks": [h] }],
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

/// Hermes splits `command` with `shlex` rather than a shell, which reads the same quoting.
/// No `type`: every Hermes `hooks:` entry is a command.
fn hermes(command: &str) -> Value {
    let entry =
        json!({ "command": harness_flag(command, Harness::Hermes), "timeout": TIMEOUT_SECS });
    let hooks: serde_json::Map<String, Value> = HERMES_EVENTS
        .iter()
        .map(|event| (event.to_string(), json!([entry.clone()])))
        .collect();
    json!({ "hooks": hooks })
}

/// The Hermes block in block-style YAML, ready to paste under or merge into `config.yaml`.
///
/// Written by hand rather than with a YAML library: the shape is fixed, and a JSON string
/// literal is a valid YAML double-quoted scalar, so the command needs no YAML escaping of its
/// own.
fn hermes_yaml(block: &Value) -> String {
    let mut out = String::from("hooks:\n");
    for event in HERMES_EVENTS {
        out.push_str(&format!("  {event}:\n"));
        for entry in block["hooks"][event].as_array().into_iter().flatten() {
            out.push_str(&format!("    - command: {}\n", entry["command"]));
            out.push_str(&format!("      timeout: {}\n", entry["timeout"]));
        }
    }
    out
}

/// The pi extension, with the hook path baked in as a JSON string literal, which is also a
/// valid TypeScript one.
pub fn pi_extension(hook_path: &str) -> String {
    include_str!("pi_extension.ts").replace(
        "__LOGBEAR_HOOK__",
        &serde_json::to_string(hook_path).expect("a string serialises"),
    )
}

/// Path to this binary, unquoted.
///
/// Claude Code and Codex run command hooks through a shell (`sh -c` / `$SHELL -lc`) and
/// Hermes through `shlex`, so [`render`] quotes it for them; pi's extension passes it to
/// `spawn` as one argument and must not see the quotes.
pub fn self_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "logbear-hook".into())
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
        let v = emit(Harness::ClaudeCode, "/usr/local/bin/logbear-hook").unwrap();
        let stop = &v["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(stop["type"], "command");
        assert_eq!(stop["command"], "/usr/local/bin/logbear-hook --harness claude");
        assert_eq!(v["hooks"]["Stop"][0]["matcher"], "");
        // A subagent that finishes over unseen errors hands them to its parent as "done".
        assert_eq!(
            v["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
            "/usr/local/bin/logbear-hook --harness claude"
        );
        assert!(v["hooks"]["PostToolUse"][0]["matcher"]
            .as_str()
            .unwrap()
            .contains("Bash"));
    }

    #[test]
    fn codex_block_carries_only_the_keys_codex_allows() {
        let v = emit(Harness::Codex, "/usr/local/bin/logbear-hook").unwrap();
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
            let v = emit(harness, "/usr/local/bin/logbear-hook").unwrap();
            assert!(
                v["hooks"]["SessionStart"][0]["hooks"][0]["command"].is_string(),
                "{harness:?} must register SessionStart"
            );
        }
    }

    #[test]
    fn hermes_yaml_quotes_a_path_with_spaces_for_shlex() {
        let text = render(Harness::Hermes, "/Applications/My App.app/logbear-hook");
        assert!(
            text.contains(
                r#"- command: "'/Applications/My App.app/logbear-hook' --harness hermes""#
            ),
            "{text}"
        );
        assert_eq!(text.matches("timeout: 10").count(), 2, "{text}");
    }

    #[test]
    fn the_pi_extension_gets_the_raw_path_not_a_shell_quoted_one() {
        let text = render(Harness::Pi, "/Applications/My App.app/logbear-hook");
        assert!(text.contains(r#"const HOOK: string = "/Applications/My App.app/logbear-hook";"#));
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

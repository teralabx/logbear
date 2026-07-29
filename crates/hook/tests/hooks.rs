//! Golden-payload tests for both harnesses.
//!
//! The payloads in `tests/payloads/` are transcribed from each vendor's documented and
//! generated schemas, so a wire-format change fails here rather than silently.

use logbear_core::model::{Boundaries, Entry, EntryId, EntryView, Level};
use logbear_core::proto::{AckKind, ProjectRef, UnreadState};
use logbear_core::{Error, Result};
use logbear_hook::payload::{self, Event, Harness};
use logbear_hook::{respond, Daemon};
use serde_json::Value;

// --------------------------------------------------------------------------- fixtures

fn golden(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/payloads")
        .join(format!("{name}.json"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"))
}

fn entry(id: EntryId, new_signature: bool, text: &str, template: &str) -> EntryView {
    EntryView {
        entry: Entry {
            id,
            run_id: 42,
            start_line: id * 10,
            end_line: id * 10 + 4,
            captured_at: logbear_core::now_micros() - 40_000_000,
            level: Level::Error,
            evidence: logbear_core::model::Evidence::Structural,
            is_error: true,
            error_reason: Some("structural:elixir_exception".into()),
            fingerprint_id: Some(id),
            boundaries: Boundaries::Exact,
        },
        text: text.into(),
        template: Some(template.into()),
        is_new_signature: new_signature,
    }
}

/// Two unread errors, budget intact, push on.
fn unread() -> UnreadState {
    UnreadState {
        new_errors: 2,
        new_signatures: 1,
        oldest_at: Some(logbear_core::now_micros() - 40_000_000),
        newest_entry_id: Some(1002),
        blocks_issued: 0,
        blocks_remaining: 3,
        change_blocks_remaining: 2,
        push_enabled: true,
        unseen_lines: 0,
        server_running: false,
        entries: vec![
            entry(
                1001,
                true,
                "** (Ecto.NoResultsError) expected at least one result but got none in query:\n    (demo 0.1.0) lib/demo/repo.ex:12: Demo.Repo.get!/2",
                "** (Ecto.NoResultsError) expected at least one result but got none in query",
            ),
            entry(
                1002,
                false,
                "[error] GenServer Demo.Worker terminating\n    ** (MatchError) no match of right hand side value: nil",
                "[error] GenServer <*> terminating",
            ),
        ],
    }
}

#[derive(Default)]
struct FakeDaemon {
    state: UnreadState,
    unread_fails: bool,
    ack_fails: bool,
    acks: Vec<(Option<EntryId>, AckKind)>,
    seen: Vec<(String, String, String)>,
}

impl FakeDaemon {
    fn with(state: UnreadState) -> FakeDaemon {
        FakeDaemon {
            state,
            ..Default::default()
        }
    }
}

impl Daemon for FakeDaemon {
    fn unread(&mut self, p: ProjectRef, session_id: &str, harness: &str) -> Result<UnreadState> {
        if let ProjectRef::Path(path) = &p {
            self.seen
                .push((path.clone(), session_id.into(), harness.into()));
        }
        if self.unread_fails {
            return Err(Error::DaemonUnreachable("/nope.sock".into()));
        }
        Ok(self.state.clone())
    }

    fn ack(
        &mut self,
        _p: ProjectRef,
        _s: &str,
        _h: &str,
        entry_id: Option<EntryId>,
        kind: AckKind,
    ) -> Result<UnreadState> {
        self.acks.push((entry_id, kind));
        if self.ack_fails {
            return Err(Error::other("write refused"));
        }
        Ok(UnreadState::default())
    }
}

fn run(name: &str, forced: Option<Harness>, daemon: &mut FakeDaemon) -> Value {
    let p = payload::parse(&golden(name), forced).expect("golden payloads must parse");
    respond(&p, daemon)
}

// --------------------------------------------------------------------------- parsing

#[test]
fn both_harnesses_expose_the_two_fields_routing_needs() {
    for (name, harness, event) in [
        ("claude_stop", Harness::ClaudeCode, Event::Stop),
        ("claude_post_tool_use", Harness::ClaudeCode, Event::PostToolUse),
        ("codex_stop", Harness::Codex, Event::Stop),
        ("codex_subagent_stop", Harness::Codex, Event::SubagentStop),
        ("codex_post_tool_use", Harness::Codex, Event::PostToolUse),
    ] {
        let p = payload::parse(&golden(name), None).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(p.cwd, "/home/dev/demo", "{name}");
        assert!(!p.session_id.is_empty(), "{name}");
        assert_eq!(p.event, event, "{name}");
        assert_eq!(p.harness, harness, "{name} auto-detection");
    }
}

#[test]
fn the_reentrancy_flag_is_read() {
    assert!(!payload::parse(&golden("claude_stop"), None)
        .unwrap()
        .stop_hook_active);
    assert!(payload::parse(&golden("claude_stop_reentrant"), None)
        .unwrap()
        .stop_hook_active);
}

#[test]
fn an_unusable_payload_is_refused_rather_than_guessed() {
    assert!(payload::parse("not json", None).is_none());
    assert!(payload::parse("[]", None).is_none());
    assert!(payload::parse(r#"{"cwd":"/tmp"}"#, None).is_none());
    assert!(payload::parse(r#"{"session_id":"","cwd":"/tmp"}"#, None).is_none());
}

// --------------------------------------------------------------------------- allow

#[test]
fn nothing_unread_is_invisible() {
    let mut d = FakeDaemon::with(UnreadState {
        push_enabled: true,
        blocks_remaining: 3,
        ..Default::default()
    });
    for name in ["claude_stop", "codex_stop", "claude_post_tool_use"] {
        assert_eq!(run(name, None, &mut d), serde_json::json!({}), "{name}");
    }
    assert!(d.acks.is_empty(), "an allow must not write anything");
}

#[test]
fn push_disabled_for_the_project_always_allows() {
    let mut d = FakeDaemon::with(UnreadState {
        push_enabled: false,
        ..unread()
    });
    assert_eq!(run("claude_stop", None, &mut d), serde_json::json!({}));
    assert_eq!(run("codex_stop", None, &mut d), serde_json::json!({}));
    assert!(d.acks.is_empty());
}

#[test]
fn an_event_we_did_not_register_for_allows() {
    let mut d = FakeDaemon::with(unread());
    let p = payload::parse(
        r#"{"session_id":"s","cwd":"/home/dev/demo","hook_event_name":"PreCompact"}"#,
        Some(Harness::ClaudeCode),
    )
    .unwrap();
    assert_eq!(respond(&p, &mut d), serde_json::json!({}));
}

// --------------------------------------------------------------------------- block

#[test]
fn stop_with_new_errors_blocks_and_says_what_and_where() {
    let mut d = FakeDaemon::with(unread());
    let out = run("claude_stop", None, &mut d);

    assert_eq!(out["decision"], "block");
    let reason = out["reason"].as_str().expect("a reason");
    assert!(reason.contains("2 new errors"), "{reason}");
    assert!(reason.contains("1 distinct signature"), "{reason}");
    assert!(reason.contains("Ecto.NoResultsError"), "{reason}");
    assert!(reason.contains("GenServer Demo.Worker terminating"), "{reason}");
    assert!(reason.contains("signature: [error] GenServer <*> terminating"), "{reason}");
    assert!(reason.contains("NEW SIGNATURE"), "{reason}");
    assert!(reason.contains("recurring"), "{reason}");
    assert!(reason.contains("run 42"), "{reason}");
    // Codex denies unknown properties on Stop, so nothing else may leak in.
    let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["decision", "reason"]);
}

#[test]
fn codex_and_claude_block_identically() {
    let mut a = FakeDaemon::with(unread());
    let mut b = FakeDaemon::with(unread());
    assert_eq!(
        run("claude_stop", None, &mut a)["decision"],
        run("codex_stop", None, &mut b)["decision"]
    );
    assert_eq!(a.acks, b.acks);
}

#[test]
fn subagent_stop_blocks_too() {
    let mut d = FakeDaemon::with(unread());
    assert_eq!(run("codex_subagent_stop", None, &mut d)["decision"], "block");
}

/// The cursor advances when the block is issued, and `record_block` lets the daemon record
/// the fingerprints so the same signature never blocks this session twice.
#[test]
fn issuing_a_block_spends_the_budget_and_advances_the_cursor() {
    let mut d = FakeDaemon::with(unread());
    run("claude_stop", None, &mut d);
    assert_eq!(d.acks, vec![(Some(1002), AckKind::StopBlock)]);
}

#[test]
fn the_project_and_session_reach_the_daemon_unmangled() {
    let mut d = FakeDaemon::with(unread());
    run("codex_stop", None, &mut d);
    assert_eq!(
        d.seen,
        vec![(
            "/home/dev/demo".to_string(),
            "codex-sess-0001".to_string(),
            "codex".to_string()
        )]
    );
}

// --------------------------------------------------------------------------- degrade

#[test]
fn past_the_block_budget_claude_degrades_to_context() {
    let mut d = FakeDaemon::with(UnreadState {
        blocks_issued: 3,
        blocks_remaining: 0,
        ..unread()
    });
    let out = run("claude_stop", None, &mut d);

    assert!(out.get("decision").is_none(), "must not block: {out}");
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "Stop");
    let ctx = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext");
    assert!(ctx.contains("2 new errors"), "{ctx}");
    assert!(d.acks.is_empty(), "a degraded response must not spend budget");
}

/// Codex's `Stop` output schema has no `hookSpecificOutput`, so a degraded stop can only
/// reach the human. Emitting `additionalContext` anyway would fail the whole hook.
#[test]
fn past_the_block_budget_codex_degrades_to_a_user_visible_warning() {
    let mut d = FakeDaemon::with(UnreadState {
        blocks_remaining: 0,
        ..unread()
    });
    let out = run("codex_stop", None, &mut d);

    assert!(out.get("hookSpecificOutput").is_none(), "{out}");
    assert!(out.get("decision").is_none(), "{out}");
    assert!(out["systemMessage"].as_str().unwrap().contains("2 new errors"));
}

/// If the watermark did not move, blocking would re-fire on the same errors forever.
#[test]
fn a_failed_ack_downgrades_the_block_instead_of_looping() {
    let mut d = FakeDaemon {
        ack_fails: true,
        ..FakeDaemon::with(unread())
    };
    let out = run("claude_stop", None, &mut d);
    assert!(out.get("decision").is_none(), "must not block: {out}");
    assert!(out["hookSpecificOutput"]["additionalContext"].is_string());
}

// --------------------------------------------------------------------------- drip

/// A recurring error was already there before this edit, so it is worth mentioning and not
/// worth interrupting for.
#[test]
fn post_tool_use_with_only_recurring_errors_stays_a_short_advisory() {
    for (name, harness) in [
        ("claude_post_tool_use", Harness::ClaudeCode),
        ("codex_post_tool_use", Harness::Codex),
    ] {
        let mut d = FakeDaemon::with(UnreadState {
            new_signatures: 0,
            ..unread()
        });
        let out = run(name, Some(harness), &mut d);

        assert!(out.get("decision").is_none(), "{name} must not block: {out}");
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        let ctx = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: {out}"));
        // Fires after every edit, so it must stay a line rather than a stack trace.
        assert!(ctx.len() < 400, "{name}: {} chars", ctx.len());
        assert!(ctx.contains("2 new errors"), "{name}: {ctx}");
        assert!(
            d.acks.is_empty(),
            "{name}: an advisory must not advance the error cursor, or the Stop hook \
             loses the errors it exists to surface"
        );
    }
}

/// The escalation: a signature this project has never produced, arriving immediately after
/// an edit, is almost certainly that edit.
#[test]
fn post_tool_use_escalates_for_a_signature_the_project_has_never_seen() {
    let mut d = FakeDaemon::with(unread());
    let out = run("claude_post_tool_use", Some(Harness::ClaudeCode), &mut d);

    assert_eq!(out["decision"], "block", "a new signature should interrupt");
    let reason = out["reason"].as_str().expect("a reason");
    assert!(reason.contains("NEW SIGNATURE"), "{reason}");
    assert_eq!(
        d.acks,
        vec![(Some(1002), AckKind::ChangeBlock)],
        "it must charge the change budget, never the Stop gate's"
    );
}

/// Once the change budget is gone the same errors must still be reported, just without
/// interrupting: a cap that turned into silence would hide them entirely.
#[test]
fn an_exhausted_change_budget_degrades_to_advisory() {
    let mut d = FakeDaemon::with(UnreadState {
        change_blocks_remaining: 0,
        ..unread()
    });
    let out = run("claude_post_tool_use", Some(Harness::ClaudeCode), &mut d);

    assert!(out.get("decision").is_none(), "must not block: {out}");
    assert!(out["hookSpecificOutput"]["additionalContext"].is_string());
    assert!(d.acks.is_empty());
}

// --------------------------------------------------------------------------- verify loop

#[test]
fn an_edit_with_unseen_server_output_asks_the_agent_to_look() {
    let mut d = FakeDaemon::with(UnreadState {
        push_enabled: true,
        server_running: true,
        unseen_lines: 4,
        ..Default::default()
    });
    let out = run("claude_post_tool_use", Some(Harness::ClaudeCode), &mut d);

    let ctx = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a nudge: {out}"));
    assert!(ctx.contains('4'), "the count belongs in the message: {ctx}");
    assert!(ctx.contains("logbear logs"), "it must say how to look: {ctx}");
    assert_eq!(
        d.acks,
        vec![(None, AckKind::Nudge)],
        "the nudge cursor must advance, or this repeats after every later edit"
    );
}

#[test]
fn an_edit_with_a_silent_server_says_nothing() {
    let mut d = FakeDaemon::with(UnreadState {
        push_enabled: true,
        server_running: true,
        unseen_lines: 0,
        ..Default::default()
    });
    assert_eq!(
        run("claude_post_tool_use", Some(Harness::ClaudeCode), &mut d),
        serde_json::json!({})
    );
    assert!(d.acks.is_empty());
}

#[test]
fn an_edit_with_nothing_running_says_nothing() {
    let mut d = FakeDaemon::with(UnreadState {
        push_enabled: true,
        server_running: false,
        unseen_lines: 12,
        ..Default::default()
    });
    assert_eq!(
        run("claude_post_tool_use", Some(Harness::ClaudeCode), &mut d),
        serde_json::json!({}),
        "nothing is running, so there is nothing to verify against"
    );
}

// --------------------------------------------------------------------------- intent

#[test]
fn an_uncaptured_dev_server_is_named_with_its_wrapped_form() {
    let mut d = FakeDaemon::with(UnreadState {
        push_enabled: true,
        ..Default::default()
    });
    let out = run("claude_bash_dev_server", Some(Harness::ClaudeCode), &mut d);

    let ctx = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a nudge: {out}"));
    assert!(ctx.contains("logbear npm run dev"), "must show the fix: {ctx}");
    assert!(d.acks.is_empty(), "an intent nudge reads no entries");
}

/// Codex sends argv, and wraps the real command in `bash -lc`. `mix compile` finishes, so
/// nothing should be said about it either way.
#[test]
fn a_codex_argv_command_is_understood_rather_than_ignored() {
    let p = payload::parse(&golden("codex_post_tool_use"), Some(Harness::Codex)).unwrap();
    assert_eq!(p.command.as_deref(), Some("mix compile"));
}

#[test]
fn a_claude_bash_command_is_read_as_a_string() {
    let p = payload::parse(&golden("claude_bash_dev_server"), Some(Harness::ClaudeCode)).unwrap();
    assert_eq!(p.command.as_deref(), Some("npm run dev"));
}

// --------------------------------------------------------------------------- session start

/// The case the rule exists for: a project logbear has never captured. The daemon answers
/// that it knows nothing about it, and that must not swallow the one piece of advice that
/// would get the next server captured.
#[test]
fn session_start_still_speaks_on_a_project_the_daemon_has_never_seen() {
    let mut d = FakeDaemon {
        unread_fails: true,
        ..FakeDaemon::with(UnreadState::default())
    };
    let out = run("claude_session_start", Some(Harness::ClaudeCode), &mut d);
    let ctx = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("expected the standing rule: {out}"));
    assert!(ctx.contains("logbear"), "{ctx}");
}

#[test]
fn session_start_respects_an_explicit_opt_out() {
    let mut d = FakeDaemon::with(UnreadState {
        push_enabled: false,
        ..Default::default()
    });
    assert_eq!(
        run("claude_session_start", Some(Harness::ClaudeCode), &mut d),
        serde_json::json!({}),
        "push off means silence, including the standing rule"
    );
}

#[test]
fn session_start_teaches_the_agent_to_wrap_long_running_commands() {
    for harness in [Harness::ClaudeCode, Harness::Codex] {
        let mut d = FakeDaemon::with(UnreadState {
            push_enabled: true,
            ..Default::default()
        });
        let out = run("claude_session_start", Some(harness), &mut d);
        let ctx = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_else(|| panic!("{harness:?}: {out}"));
        assert!(ctx.contains("logbear npm run dev"), "{ctx}");
        assert!(d.acks.is_empty(), "saying hello writes nothing");
    }
}

// --------------------------------------------------------------------------- fail open

#[test]
fn a_daemon_error_allows() {
    let mut d = FakeDaemon {
        unread_fails: true,
        ..FakeDaemon::with(unread())
    };
    for name in ["claude_stop", "codex_stop", "claude_post_tool_use"] {
        assert_eq!(run(name, None, &mut d), serde_json::json!({}), "{name}");
    }
}

// --------------------------------------------------------------------------- the binary

/// End to end through the real binary with no daemon running, the most likely failure.
#[test]
fn the_binary_fails_open_with_no_daemon() {
    for input in [
        golden("claude_stop"),
        golden("codex_stop"),
        "not json at all".to_string(),
        String::new(),
    ] {
        let out = invoke(&["--harness", "claude"], &input);
        assert_eq!(out.status.code(), Some(0), "exit code");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            serde_json::from_str::<Value>(stdout.trim()).expect("stdout must be JSON"),
            serde_json::json!({}),
            "stdout was {stdout:?}"
        );
    }
}

#[test]
fn emit_config_prints_only_json_on_stdout() {
    for which in ["claude", "codex"] {
        let out = invoke(&["--emit-config", which], "");
        assert_eq!(out.status.code(), Some(0));
        let v: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{which}: {e}: {:?}", String::from_utf8_lossy(&out.stdout)));
        assert!(v["hooks"]["Stop"].is_array(), "{which}");
        let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.contains("logbear-hook"), "{which}: {cmd}");
        assert!(cmd.ends_with(&format!("--harness {which}")), "{which}: {cmd}");
    }
    // The Codex trust caveat goes to stderr so stdout stays pipeable.
    let codex = invoke(&["--emit-config", "codex"], "");
    assert!(String::from_utf8_lossy(&codex.stderr).contains("/hooks"));
}

fn invoke(args: &[&str], stdin: &str) -> std::process::Output {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let dir = std::env::temp_dir().join(format!("logbear-hook-test-{}", std::process::id()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_logbear-hook"))
        .args(args)
        // Point at a socket that cannot exist, so the test can never reach a real daemon.
        .env("LOGBEAR_HOME", &dir)
        .env("LOGBEAR_SOCKET", dir.join("absent.sock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning logbear-hook");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("writing stdin");
    child.wait_with_output().expect("waiting for logbear-hook")
}

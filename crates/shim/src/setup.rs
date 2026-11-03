//! `logbear setup` — connect logbear to the coding agents on this machine.
//!
//! The same steps the desktop app's Settings screen offers, driven from the terminal for
//! people who installed with Homebrew, the install script or cargo. Nothing is written until
//! the plan has been shown and confirmed; every config write is backed up and merged, never
//! overwritten, exactly as the app does it, because it is the same code.

use crate::ui::{self, Cell};
use logbear_setup::{InstallStatus, Step, StepState};
use std::io::{BufRead, IsTerminal, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    Claude,
    Codex,
    Pi,
    Hermes,
}

impl Agent {
    const ALL: [Agent; 4] = [Agent::Claude, Agent::Codex, Agent::Pi, Agent::Hermes];

    /// Its MCP step, then its hook step.
    fn steps(self) -> [Step; 2] {
        match self {
            Agent::Claude => [Step::McpClaude, Step::HookClaude],
            Agent::Codex => [Step::McpCodex, Step::HookCodex],
            Agent::Pi => [Step::McpPi, Step::HookPi],
            Agent::Hermes => [Step::McpHermes, Step::HookHermes],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Install,
    Remove,
    /// Already in the state asked for.
    Done,
    Skip(String),
}

#[derive(Debug)]
pub struct Planned {
    pub step: Step,
    pub action: Action,
    pub detail: String,
}

fn state(status: &InstallStatus, step: Step) -> &StepState {
    match step {
        Step::Path => &status.path,
        Step::Daemon => &status.daemon,
        Step::McpClaude => &status.mcp_claude,
        Step::McpCodex => &status.mcp_codex,
        Step::McpPi => &status.mcp_pi,
        Step::McpHermes => &status.mcp_hermes,
        Step::HookClaude => &status.hook_claude,
        Step::HookCodex => &status.hook_codex,
        Step::HookPi => &status.hook_pi,
        Step::HookHermes => &status.hook_hermes,
    }
}

fn title(step: Step) -> &'static str {
    match step {
        Step::Path => "logbear on PATH",
        Step::Daemon => "background daemon",
        Step::McpClaude => "MCP · Claude Code",
        Step::McpCodex => "MCP · Codex",
        Step::McpPi => "MCP · pi",
        Step::McpHermes => "MCP · Hermes",
        Step::HookClaude => "hooks · Claude Code",
        Step::HookCodex => "hooks · Codex",
        Step::HookPi => "extension · pi",
        Step::HookHermes => "hooks · Hermes",
    }
}

/// What `setup` would do, given what is installed now.
///
/// PATH is never a step here: whatever ran this command is already on it. The daemon is one
/// only when this install owns its service — Homebrew's is `brew services` — and only when
/// no agent was named, since `--agent pi` asks about pi and nothing else.
pub fn plan(status: &InstallStatus, only: &[Agent], remove: bool, homebrew: bool) -> Vec<Planned> {
    let mut out = Vec::new();

    if only.is_empty() {
        let daemon = state(status, Step::Daemon);
        let action = if homebrew {
            Action::Skip(if remove {
                "Homebrew runs it: brew services stop logbear".into()
            } else {
                "Homebrew runs it: brew services start logbear".into()
            })
        } else {
            wanted(daemon, remove)
        };
        out.push(Planned {
            step: Step::Daemon,
            action,
            detail: daemon.detail.clone(),
        });
    }

    let agents: Vec<Agent> = if only.is_empty() {
        Agent::ALL.to_vec()
    } else {
        Agent::ALL
            .into_iter()
            .filter(|a| only.contains(a))
            .collect()
    };
    for agent in agents {
        for step in agent.steps() {
            let s = state(status, step);
            let action = if s.available {
                wanted(s, remove)
            } else {
                Action::Skip(s.detail.clone())
            };
            out.push(Planned {
                step,
                action,
                detail: s.detail.clone(),
            });
        }
    }
    out
}

fn wanted(s: &StepState, remove: bool) -> Action {
    match (remove, s.installed) {
        (false, false) => Action::Install,
        (true, true) => Action::Remove,
        _ => Action::Done,
    }
}

pub fn run(only: Vec<Agent>, yes: bool, remove: bool) -> Result<i32, String> {
    let steps = plan(
        &logbear_setup::status(),
        &only,
        remove,
        logbear_setup::homebrew_managed(),
    );
    print!("{}", render_plan(&steps));

    let todo: Vec<&Planned> = steps
        .iter()
        .filter(|p| matches!(p.action, Action::Install | Action::Remove))
        .collect();
    if todo.is_empty() {
        println!("\n{}", ui::clear("nothing to do"));
        return Ok(0);
    }

    if !yes {
        if !std::io::stdin().is_terminal() {
            println!("\nNothing changed. Re-run with --yes to apply this without a prompt.");
            return Ok(1);
        }
        let verb = if remove { "Remove" } else { "Make" };
        print!(
            "\n{verb} these {} change{}? [y/N] ",
            todo.len(),
            plural(todo.len())
        );
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing changed.");
            return Ok(1);
        }
    }

    println!();
    let mut failures = 0;
    for p in todo {
        let result = match p.action {
            Action::Remove => logbear_setup::uninstall(p.step),
            _ => logbear_setup::install(p.step),
        };
        match result {
            Ok(s) => {
                let backup = s
                    .backup
                    .map(|b| format!(" (backup: {b})"))
                    .unwrap_or_default();
                println!(
                    "{} {}  {}{}",
                    ui::clear("ok "),
                    title(p.step),
                    ui::ash(&s.detail),
                    ui::ash(&backup)
                );
            }
            Err(e) => {
                failures += 1;
                println!("{} {}  {e}", ui::flare("err"), title(p.step));
            }
        }
    }

    if !remove {
        for note in notes(&steps) {
            println!("\n{note}");
        }
    }
    Ok(if failures == 0 { 0 } else { 1 })
}

/// What the agent itself still needs from the user. Each one fails quietly otherwise: an
/// untrusted Codex or Hermes hook never fires, and an open pi session never loads the new
/// extension.
fn notes(steps: &[Planned]) -> Vec<&'static str> {
    let installed = |step: Step| {
        steps
            .iter()
            .any(|p| p.step == step && p.action == Action::Install)
    };
    let mut out = Vec::new();
    if installed(Step::HookCodex) {
        out.push("Codex: run /hooks inside Codex and trust the logbear hook. Until you do it never runs.");
    }
    if installed(Step::HookHermes) {
        out.push("Hermes: approve both logbear hooks when Hermes asks, or review them with `hermes hooks`.");
    }
    if installed(Step::HookPi) || installed(Step::McpPi) {
        out.push("pi: run /reload in any open pi session.");
    }
    if installed(Step::McpClaude) || installed(Step::HookClaude) {
        out.push("Claude Code: restart open sessions to pick up the MCP server and hooks.");
    }
    out
}

fn render_plan(steps: &[Planned]) -> String {
    let rows: Vec<Vec<Cell>> = steps
        .iter()
        .map(|p| {
            let (label, colour, detail) = match &p.action {
                Action::Install => ("add", ui::EMBER, p.detail.clone()),
                Action::Remove => ("remove", ui::EMBER, p.detail.clone()),
                Action::Done => ("done", ui::CLEAR, p.detail.clone()),
                Action::Skip(why) => ("skip", ui::ASH, why.clone()),
            };
            vec![
                Cell::coloured(label, colour),
                Cell::plain(title(p.step)),
                Cell::coloured(detail, ui::ASH),
            ]
        })
        .collect();
    ui::table(&["", "STEP", "DETAIL"], &rows)
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(installed: bool, available: bool) -> StepState {
        StepState {
            installed,
            available,
            detail: if available {
                "here".into()
            } else {
                "not detected".into()
            },
            backup: None,
        }
    }

    /// Claude Code installed on the machine, nothing set up; nothing else present.
    fn fresh() -> InstallStatus {
        InstallStatus {
            path: st(true, true),
            daemon: st(false, true),
            mcp_claude: st(false, true),
            mcp_codex: st(false, false),
            mcp_pi: st(false, false),
            mcp_hermes: st(false, false),
            hook_claude: st(false, true),
            hook_codex: st(false, false),
            hook_pi: st(false, false),
            hook_hermes: st(false, false),
        }
    }

    fn action(plan: &[Planned], step: Step) -> Option<&Action> {
        plan.iter().find(|p| p.step == step).map(|p| &p.action)
    }

    #[test]
    fn a_fresh_machine_gets_the_daemon_and_every_agent_it_has() {
        let p = plan(&fresh(), &[], false, false);
        assert_eq!(action(&p, Step::Daemon), Some(&Action::Install));
        assert_eq!(action(&p, Step::McpClaude), Some(&Action::Install));
        assert_eq!(action(&p, Step::HookClaude), Some(&Action::Install));
        assert!(
            matches!(action(&p, Step::HookCodex), Some(Action::Skip(why)) if why == "not detected")
        );
        assert_eq!(
            action(&p, Step::Path),
            None,
            "PATH is never setup's to change"
        );
    }

    #[test]
    fn homebrew_keeps_the_daemon_service_for_itself() {
        let p = plan(&fresh(), &[], false, true);
        assert!(
            matches!(action(&p, Step::Daemon), Some(Action::Skip(why)) if why.contains("brew services start"))
        );
    }

    #[test]
    fn naming_an_agent_plans_that_agent_alone() {
        let mut s = fresh();
        s.mcp_pi = st(false, true);
        s.hook_pi = st(false, true);
        let p = plan(&s, &[Agent::Pi], false, false);
        let steps: Vec<Step> = p.iter().map(|p| p.step).collect();
        assert_eq!(steps, vec![Step::McpPi, Step::HookPi]);
        assert!(p.iter().all(|p| p.action == Action::Install));
    }

    #[test]
    fn what_is_already_there_is_left_alone() {
        let mut s = fresh();
        s.mcp_claude = st(true, true);
        let p = plan(&s, &[], false, false);
        assert_eq!(action(&p, Step::McpClaude), Some(&Action::Done));
        assert_eq!(action(&p, Step::HookClaude), Some(&Action::Install));
    }

    #[test]
    fn remove_undoes_only_what_is_installed() {
        let mut s = fresh();
        s.daemon = st(true, true);
        s.hook_claude = st(true, true);
        let p = plan(&s, &[], true, false);
        assert_eq!(action(&p, Step::Daemon), Some(&Action::Remove));
        assert_eq!(action(&p, Step::HookClaude), Some(&Action::Remove));
        assert_eq!(action(&p, Step::McpClaude), Some(&Action::Done));
    }

    #[test]
    fn trust_reminders_follow_the_hooks_that_need_them() {
        let mut s = fresh();
        s.hook_codex = st(false, true);
        s.mcp_codex = st(false, true);
        let p = plan(&s, &[Agent::Codex], false, false);
        let n = notes(&p);
        assert!(n.iter().any(|n| n.contains("/hooks")), "{n:?}");
        assert!(!n.iter().any(|n| n.contains("Hermes")), "{n:?}");
    }
}

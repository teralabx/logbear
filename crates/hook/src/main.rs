//! `runlog-hook` — read the harness's JSON on stdin, write JSON on stdout, exit 0.
//!
//! Always exit 0. Both harnesses give non-zero exit codes their own meaning (Claude Code
//! reads exit 2 as a block with stderr as the reason; Codex the same), so a crash that
//! happened to exit 2 would inject a Rust panic message into the user's agent as if it were
//! a considered decision. Exit 0 with `{}` is the only safe failure.

use runlog_hook::config;
use runlog_hook::payload::{self, Harness};
use serde_json::Value;
use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut forced: Option<Harness> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--emit-config" => {
                let which = args.get(i + 1).map(String::as_str).unwrap_or("");
                return emit_config(which);
            }
            "--harness" => {
                match args.get(i + 1).and_then(|s| Harness::parse(s)) {
                    Some(h) => forced = Some(h),
                    None => {
                        eprintln!("runlog-hook: --harness takes `claude` or `codex`");
                        // Still exit 0: a misconfigured hook must not break the agent.
                        return emit(&runlog_hook::allow());
                    }
                }
                i += 1;
            }
            "--version" | "-V" => {
                println!("runlog-hook {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => return help(),
            other => eprintln!("runlog-hook: ignoring unknown argument {other:?}"),
        }
        i += 1;
    }

    emit(&run(forced));
}

/// Everything that can go wrong here resolves to the same answer, so it is one funnel.
fn run(forced: Option<Harness>) -> Value {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("runlog-hook: could not read stdin ({e})");
        return runlog_hook::allow();
    }

    let payload = match payload::parse(&input, forced) {
        Some(p) => p,
        None => {
            eprintln!("runlog-hook: unrecognised hook payload");
            return runlog_hook::allow();
        }
    };

    let mut client = match runlog_hook::connect() {
        Some(c) => c,
        None => {
            eprintln!("runlog-hook: runlogd is not running");
            return runlog_hook::allow();
        }
    };

    runlog_hook::respond(&payload, &mut client)
}

fn emit(value: &Value) {
    // Nothing but this ever reaches stdout. Codex treats unparseable stdout on `Stop` as a
    // hard hook failure, so a stray diagnostic here would break the user's agent.
    println!("{value}");
}

fn emit_config(which: &str) {
    let harness = match Harness::parse(which) {
        Some(h) => h,
        None => {
            eprintln!("runlog-hook: --emit-config takes `claude` or `codex`");
            std::process::exit(2);
        }
    };
    let block = config::emit(harness, &config::self_command());
    println!(
        "{}",
        serde_json::to_string_pretty(&block).expect("a literal json value serialises")
    );
    if harness == Harness::Codex {
        // stdout stays pure JSON so this can be piped straight into a merge; the caveat
        // that costs users the most time goes to stderr for the installer and `doctor`.
        eprintln!("note: {}", config::CODEX_TRUST_NOTE);
    }
}

fn help() {
    println!(
        "runlog-hook {}\n\n\
         Invoked by Claude Code and Codex, not by hand. Reads the harness's hook payload on\n\
         stdin and writes a hook response on stdout.\n\n\
         Options:\n  \
           --harness <claude|codex>      Which harness is calling. Auto-detected if omitted.\n  \
           --emit-config <claude|codex>  Print the config block to merge into that harness.\n  \
           --version, --help\n\n\
         Config locations:\n  \
           claude   ~/.claude/settings.json\n  \
           codex    ~/.codex/hooks.json  (or [hooks] in ~/.codex/config.toml)\n",
        env!("CARGO_PKG_VERSION")
    );
}

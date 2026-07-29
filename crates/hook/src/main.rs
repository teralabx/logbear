//! `logbear-hook` — read the harness's JSON on stdin, write JSON on stdout, exit 0.
//!
//! Always exit 0. Both harnesses read exit 2 as a block with stderr as the reason, so a
//! crash that happened to exit 2 would inject a panic message into the agent as if it were
//! a considered decision.

use logbear_hook::config;
use logbear_hook::payload::{self, Harness};
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
                        eprintln!("logbear-hook: --harness takes `claude` or `codex`");
                        // Still allow: a misconfigured hook must not break the agent.
                        return emit(&logbear_hook::allow());
                    }
                }
                i += 1;
            }
            "--version" | "-V" => {
                println!("logbear-hook {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => return help(),
            other => eprintln!("logbear-hook: ignoring unknown argument {other:?}"),
        }
        i += 1;
    }

    emit(&run(forced));
}

/// Read the payload, ask the daemon, and return the response. Every failure allows.
fn run(forced: Option<Harness>) -> Value {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("logbear-hook: could not read stdin ({e})");
        return logbear_hook::allow();
    }

    let payload = match payload::parse(&input, forced) {
        Some(p) => p,
        None => {
            eprintln!("logbear-hook: unrecognised hook payload");
            return logbear_hook::allow();
        }
    };

    let mut client = match logbear_hook::connect() {
        Some(c) => c,
        None => {
            eprintln!("logbear-hook: logbeard is not running");
            return logbear_hook::allow();
        }
    };

    logbear_hook::respond(&payload, &mut client)
}

fn emit(value: &Value) {
    // The only write to stdout: Codex treats unparseable stdout on `Stop` as a hard hook
    // failure, so a stray diagnostic here would break the agent.
    println!("{value}");
}

fn emit_config(which: &str) {
    let harness = match Harness::parse(which) {
        Some(h) => h,
        None => {
            eprintln!("logbear-hook: --emit-config takes `claude` or `codex`");
            std::process::exit(2);
        }
    };
    let block = config::emit(harness, &config::self_command());
    println!(
        "{}",
        serde_json::to_string_pretty(&block).expect("a literal json value serialises")
    );
    if harness == Harness::Codex {
        // stdout stays pure JSON so it can be piped into a merge; the trust caveat goes to
        // stderr for the installer and `doctor`.
        eprintln!("note: {}", config::CODEX_TRUST_NOTE);
    }
}

fn help() {
    println!(
        "logbear-hook {}\n\n\
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

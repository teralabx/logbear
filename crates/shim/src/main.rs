//! `runlog` — the user-facing binary.
//!
//! `runlog <cmd>` wraps a command under a pty and streams a copy of its output to the
//! daemon ([`wrap`]); every other verb queries what has already been captured ([`cmds`]).
//! [`cli`] decides which by exact match against a closed list of subcommand names, because
//! the primary verb is whatever the user typed.

mod cli;
mod cmds;
mod doctor;
mod sink;
mod term;
mod ui;
mod wrap;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::classify(&args) {
        // Diverges: the child's exit code has to be this process's exit code.
        cli::Dispatch::Wrap(w) => wrap::run(w),
        cli::Dispatch::Subcommand => std::process::exit(cli::run_subcommand(&args)),
    }
}

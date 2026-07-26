//! `runlog` — the binary the user types.
//!
//! Two jobs in one executable. `runlog <cmd>` wraps a command under a pty and streams a
//! copy of its output to the daemon ([`wrap`]); everything else is a query against what has
//! already been captured ([`cmds`]). Which of the two you get is decided in [`cli`], by
//! exact match against a closed list of subcommand names, because the primary verb is
//! whatever the user typed and clap cannot be allowed to reject it.
//!
//! Deliberately thin. There is no parsing, no SQLite and no network protocol here — those
//! live in `runlog-core` and in the daemon. The shim's entire job is to be the thing that
//! cannot fail: it opens a pty, copies bytes to the terminal before anyone else sees them,
//! forwards signals, and propagates the exit code unchanged. Everything it does for runlog
//! happens behind a queue that is allowed to be dropped on the floor.

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
        // Diverges: the wrapped child's exit code has to be this process's exit code, so
        // there is nothing to return here.
        cli::Dispatch::Wrap(w) => wrap::run(w),
        cli::Dispatch::Subcommand => std::process::exit(cli::run_subcommand(&args)),
    }
}

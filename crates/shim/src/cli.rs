//! Argument dispatch.
//!
//! The primary verb is whatever the user typed: `logbear mix phx.server` wraps mix, and
//! `logbear logs` shows logs. clap cannot own argv, because it would reject `logbear test` as
//! an unknown subcommand.
//!
//! So argv is classified here by exact match against a closed list of subcommand names, and
//! only a recognised subcommand reaches clap; everything else is a command to run. Both
//! `logbear -- logs` and a leading wrap flag (`--no-pty`, `--force`) force the wrap reading.

use clap::{Parser, Subcommand};

/// The closed list. A name added here is taken away from the wrap path, so the names are
/// deliberately ones nobody has an executable called.
pub const SUBCOMMANDS: &[&str] = &[
    "cat",
    "logs",
    "runs",
    "diff",
    "status",
    "doctor",
    "setup",
    "updates",
    "query",
    "sql-schema",
    "squash",
];

const TOP_LEVEL_FLAGS: &[&str] = &["-h", "--help", "-V", "--version", "help"];

#[derive(Debug, PartialEq)]
pub enum Dispatch {
    /// Hand the whole argv to clap.
    Subcommand,
    Wrap(WrapArgs),
}

#[derive(Debug, Default, PartialEq)]
pub struct WrapArgs {
    pub command: Vec<String>,
    /// Pipes instead of a pty. Degraded: libc fully buffers a pipe at 4 KB, so per-line
    /// arrival times stop being real and interactive tooling stops working.
    pub no_pty: bool,
    /// Capture even a denylisted command.
    pub force: bool,
}

pub fn classify(args: &[String]) -> Dispatch {
    if args.is_empty() {
        return Dispatch::Subcommand; // clap prints the help
    }
    if TOP_LEVEL_FLAGS.contains(&args[0].as_str()) {
        return Dispatch::Subcommand;
    }

    let mut wrap = WrapArgs::default();
    let mut i = 0;
    let mut forced = false;
    while i < args.len() {
        match args[i].as_str() {
            "--no-pty" => {
                wrap.no_pty = true;
                forced = true;
                i += 1;
            }
            "--force" => {
                wrap.force = true;
                forced = true;
                i += 1;
            }
            "--" => {
                forced = true;
                i += 1;
                break;
            }
            _ => break,
        }
    }

    let rest = &args[i..];
    if rest.is_empty() {
        return Dispatch::Subcommand;
    }
    // A leading wrap flag or `--` settles the ambiguity: a subcommand name after one is a
    // program to run.
    if !forced && SUBCOMMANDS.contains(&rest[0].as_str()) {
        return Dispatch::Subcommand;
    }
    wrap.command = rest.to_vec();
    Dispatch::Wrap(wrap)
}

// ---------------------------------------------------------------------------
// The subcommand surface
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "logbear",
    version,
    about = "Wrap a command, capture everything it emits, and push errors at your agent.",
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "Anything that is not one of the commands above is a command to wrap:\n  \
                  logbear mix phx.server\n  \
                  logbear --no-pty npm test     pipes instead of a pty (degraded timing)\n  \
                  logbear --force psql          capture even a denylisted command\n  \
                  logbear -- logs               wrap a program actually named `logs`"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Replay a run's stored bytes to stdout, ANSI intact.
    Cat {
        run_id: i64,
    },

    /// Recent entries for the current project.
    Logs {
        /// How many entries.
        #[arg(short = 'n', long = "number", default_value_t = 50)]
        number: usize,
        /// error | warn | info | debug | trace
        #[arg(long)]
        level: Option<String>,
        /// Errors only: a declared error, or a runtime exception banner.
        #[arg(long)]
        errors: bool,
        /// Only output never seen in this project before, at any level. Finds the
        /// print/IO.inspect lines you just added, which declare no level to filter on.
        #[arg(long)]
        new: bool,
        /// Only entries newer than this: 30s, 10m, 2h, 1d.
        #[arg(long)]
        since: Option<String>,
        /// Full-text search over rendered entry text.
        #[arg(long)]
        search: Option<String>,
        /// Restrict to one run.
        #[arg(long)]
        run: Option<i64>,
        #[arg(long)]
        json: bool,
    },

    /// Run history for the current project.
    Runs {
        #[arg(short = 'n', long = "number", default_value_t = 20)]
        number: usize,
        #[arg(long)]
        json: bool,
    },

    /// Diff two runs. Defaults to last-good against latest.
    Diff {
        /// A run id, or one of: latest, last-good, last-bad.
        a: Option<String>,
        b: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Daemon health, resolved project, retention usage.
    Status {
        #[arg(long)]
        json: bool,
    },

    /// Check the install: PATH, daemon, MCP registration, hooks.
    Doctor {
        #[arg(long)]
        json: bool,
    },

    /// Connect logbear to the coding agents on this machine: MCP server and hook for each of
    /// Claude Code, Codex, pi and Hermes found, plus the daemon service. Shows the plan and
    /// asks before writing anything; every config is backed up and merged.
    Setup {
        /// Only these agents. Repeatable. Without it, every agent found, and the daemon.
        #[arg(long = "agent", value_enum)]
        agents: Vec<crate::setup::Agent>,
        /// Apply without asking.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Undo instead: take logbear back out of each agent's config.
        #[arg(long)]
        remove: bool,
    },

    /// Opt-in update checks: `on`, `off`, or `check` now. Off by default; when on,
    /// `logbear doctor` asks GitHub for the latest release at most once a day.
    Updates {
        /// on | off | check. Without it, the current state.
        action: Option<String>,
    },

    /// Read-only SQL against the current project's database.
    Query {
        sql: String,
        #[arg(long)]
        json: bool,
    },

    /// Print the schema, views and worked examples an agent needs before writing SQL.
    SqlSchema,

    /// Shrink finished runs to the first occurrence of each distinct entry. Repeats that
    /// differ only in timestamps, ids or numbers are dropped; counts are kept. Retention
    /// does this on its own before evicting anything; this does it now.
    Squash {
        /// One run. Without it, every finished run in the current project.
        run_id: Option<i64>,
        #[arg(long)]
        json: bool,
    },
}

/// Runs a subcommand and returns the process exit code. clap's `--help` and `--version`
/// exit inside `parse_from`.
pub fn run_subcommand(args: &[String]) -> i32 {
    let argv = std::iter::once("logbear".to_string()).chain(args.iter().cloned());
    let cli = Cli::parse_from(argv);
    match crate::cmds::dispatch(cli.command) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn wrapped(parts: &[&str]) -> WrapArgs {
        match classify(&argv(parts)) {
            Dispatch::Wrap(w) => w,
            Dispatch::Subcommand => panic!("{parts:?} was treated as a subcommand"),
        }
    }

    fn is_subcommand(parts: &[&str]) -> bool {
        matches!(classify(&argv(parts)), Dispatch::Subcommand)
    }

    #[test]
    fn a_bare_command_is_wrapped() {
        assert_eq!(wrapped(&["mix", "phx.server"]).command, ["mix", "phx.server"]);
        assert_eq!(wrapped(&["npm", "run", "dev"]).command, ["npm", "run", "dev"]);
    }

    #[test]
    fn words_that_look_like_subcommands_but_are_not_get_wrapped() {
        // `logbear test` and `logbear run` are the classic failures: clap would reject both.
        assert_eq!(wrapped(&["test"]).command, ["test"]);
        assert_eq!(wrapped(&["run"]).command, ["run"]);
        assert_eq!(wrapped(&["make", "logs"]).command, ["make", "logs"]);
    }

    #[test]
    fn the_command_keeps_its_own_flags() {
        let w = wrapped(&["cargo", "test", "--", "--nocapture"]);
        assert_eq!(w.command, ["cargo", "test", "--", "--nocapture"]);
        assert!(!w.no_pty);
    }

    #[test]
    fn subcommands_go_to_clap() {
        assert!(is_subcommand(&["logs"]));
        assert!(is_subcommand(&["logs", "--help"]));
        assert!(is_subcommand(&["logs", "-n", "5", "--errors"]));
        assert!(is_subcommand(&["runs"]));
        assert!(is_subcommand(&["cat", "12"]));
        assert!(is_subcommand(&["diff", "last-good", "latest"]));
        assert!(is_subcommand(&["sql-schema"]));
        assert!(is_subcommand(&["doctor"]));
    }

    #[test]
    fn help_and_version_go_to_clap() {
        assert!(is_subcommand(&[]));
        assert!(is_subcommand(&["--help"]));
        assert!(is_subcommand(&["-h"]));
        assert!(is_subcommand(&["--version"]));
        assert!(is_subcommand(&["-V"]));
        assert!(is_subcommand(&["help"]));
    }

    #[test]
    fn a_double_dash_forces_the_wrap_reading() {
        assert_eq!(wrapped(&["--", "logs"]).command, ["logs"]);
        assert_eq!(wrapped(&["--", "diff", "a", "b"]).command, ["diff", "a", "b"]);
    }

    #[test]
    fn wrap_flags_are_consumed_and_imply_a_wrap() {
        let w = wrapped(&["--no-pty", "npm", "test"]);
        assert!(w.no_pty);
        assert_eq!(w.command, ["npm", "test"]);

        let w = wrapped(&["--force", "psql"]);
        assert!(w.force);
        assert_eq!(w.command, ["psql"]);

        let w = wrapped(&["--force", "--no-pty", "logs"]);
        assert!(w.force && w.no_pty);
        assert_eq!(w.command, ["logs"], "a wrap flag settles the ambiguity");
    }

    #[test]
    fn a_wrap_flag_with_nothing_after_it_is_not_a_command() {
        assert!(is_subcommand(&["--no-pty"]));
        assert!(is_subcommand(&["--"]));
    }

    #[test]
    fn the_clap_definition_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn every_clap_subcommand_is_in_the_closed_list() {
        use clap::CommandFactory;
        for sub in Cli::command().get_subcommands() {
            let name = sub.get_name();
            if name == "help" {
                continue;
            }
            assert!(
                SUBCOMMANDS.contains(&name),
                "`{name}` is a clap subcommand but argv dispatch would wrap it"
            );
        }
    }
}

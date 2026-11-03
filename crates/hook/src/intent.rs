//! What a shell command is trying to do, as far as logbear cares.
//!
//! Pure and I/O-free, like [`crate::decide`]: the rules are worth testing on their own, and a
//! classifier that needed a daemon could not be.
//!
//! The distinction that matters is process lifetime, not a fixed list of tools. A command
//! that runs until killed loses everything it emits unless logbear is holding it. A command
//! that terminates hands its output straight back to the agent, so saying anything about it
//! is noise — which is why builds and test runs are deliberately absent below.

/// The wrapper the agent should have used.
const WRAPPER: &str = "logbear";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Runs until killed, so its output is being lost.
    StartsServer { suggested: String },
    /// Reaching for output logbear already holds.
    ChasesBug,
    /// Anything else. The overwhelmingly common case, and always silent.
    Neither,
}

/// Commands that run until killed, matched on their leading words.
///
/// Leading words rather than a substring search, so `npm run dev` matches but
/// `echo npm run dev` does not.
const SERVER_HEADS: &[&[&str]] = &[
    &["vite"],
    &["next", "dev"],
    &["nuxt", "dev"],
    &["astro", "dev"],
    &["remix", "dev"],
    &["rails", "server"],
    &["rails", "s"],
    &["mix", "phx.server"],
    &["cargo", "watch"],
    &["flask", "run"],
    &["hugo", "server"],
    &["jekyll", "serve"],
    &["php", "artisan", "serve"],
];

/// Package managers whose scripts are named by convention.
const RUNNERS: &[&str] = &["npm", "pnpm", "yarn", "bun", "deno"];

/// Script names that conventionally mean "run until killed". `build` and `test` are pointedly
/// not here: they finish, and the agent already sees their output.
const LONG_LIVED_SCRIPTS: &[&str] = &["dev", "start", "serve", "watch"];

/// Programs whose whole purpose is reading back output.
const LOG_READERS: &[&str] = &["tail", "less", "journalctl"];

pub fn classify(command: &str) -> Intent {
    // Judged per segment: `cd frontend && npm run dev` starts a server, and the advice has to
    // name the server rather than the whole line.
    for segment in segments(command) {
        let words = words(segment);
        let Some(&head) = words.first() else {
            continue;
        };
        // Already captured. Nothing to say, and re-wrapping it would be wrong.
        if head == WRAPPER {
            return Intent::Neither;
        }
        if starts_server(&words) {
            return Intent::StartsServer {
                suggested: format!("{WRAPPER} {}", words.join(" ")),
            };
        }
    }
    for segment in segments(command) {
        if hunts_logs(&words(segment)) {
            return Intent::ChasesBug;
        }
    }
    Intent::Neither
}

fn starts_server(words: &[&str]) -> bool {
    if SERVER_HEADS
        .iter()
        .any(|head| words.len() >= head.len() && &words[..head.len()] == *head)
    {
        return true;
    }
    // `python manage.py runserver`, and the bare `./manage.py` form.
    if words.iter().any(|w| w.ends_with("manage.py")) && words.contains(&"runserver") {
        return true;
    }
    // `docker compose up` holds the terminal; `-d` detaches and returns immediately.
    if words.starts_with(&["docker", "compose", "up"])
        && !words.iter().any(|w| *w == "-d" || *w == "--detach")
    {
        return true;
    }
    if RUNNERS.contains(&words[0]) {
        // `npm run dev` and the `npm dev` shorthand reach the same script.
        let script = match words.get(1) {
            Some(&"run") => words.get(2),
            other => other,
        };
        return script.is_some_and(|s| LONG_LIVED_SCRIPTS.contains(s));
    }
    false
}

fn hunts_logs(words: &[&str]) -> bool {
    let Some(&program) = words.first() else {
        return false;
    };
    if LOG_READERS.contains(&program) {
        return true;
    }
    if program == "docker" && words.get(1) == Some(&"logs") {
        return true;
    }
    // `cat` and `grep` are ordinary tools; only a log file makes them a log hunt. Searching
    // source is the agent doing its job.
    if matches!(program, "cat" | "grep" | "rg" | "head") {
        return words.iter().skip(1).any(|w| is_log_path(w));
    }
    false
}

fn is_log_path(word: &str) -> bool {
    let w = word.trim_matches(['"', '\'']);
    w.ends_with(".log") || w.ends_with(".out")
}

/// Splits on the shell operators that separate whole commands.
///
/// Deliberately not a shell parser: a wrong split costs at most a missed or misdirected
/// nudge, never a wrong action, and nothing here decides whether a command runs.
fn segments(command: &str) -> Vec<&str> {
    command
        .split(['\n', ';'])
        .flat_map(|s| s.split("&&"))
        .flat_map(|s| s.split("||"))
        .flat_map(|s| s.split('|'))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

fn words(segment: &str) -> Vec<&str> {
    segment.split_whitespace().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrapped(cmd: &str) -> String {
        match classify(cmd) {
            Intent::StartsServer { suggested } => suggested,
            other => panic!("{cmd:?} should start a server, got {other:?}"),
        }
    }

    #[test]
    fn the_common_dev_servers_are_recognised() {
        assert_eq!(wrapped("npm run dev"), "logbear npm run dev");
        assert_eq!(wrapped("pnpm dev"), "logbear pnpm dev");
        assert_eq!(wrapped("bun run start"), "logbear bun run start");
        assert_eq!(wrapped("mix phx.server"), "logbear mix phx.server");
        assert_eq!(wrapped("rails server"), "logbear rails server");
        assert_eq!(
            wrapped("python manage.py runserver"),
            "logbear python manage.py runserver"
        );
        assert_eq!(wrapped("vite"), "logbear vite");
        assert_eq!(wrapped("next dev"), "logbear next dev");
        assert_eq!(wrapped("cargo watch -x run"), "logbear cargo watch -x run");
        assert_eq!(wrapped("docker compose up"), "logbear docker compose up");
    }

    #[test]
    fn a_command_already_wrapped_is_left_alone() {
        assert_eq!(classify("logbear npm run dev"), Intent::Neither);
        assert_eq!(classify("  logbear   mix phx.server"), Intent::Neither);
    }

    #[test]
    fn commands_that_finish_are_never_server_starts() {
        // These terminate and hand their output straight back to the agent, so logbear adds
        // nothing and a nudge would be pure noise.
        for cmd in [
            "npm run build",
            "npm test",
            "cargo test",
            "cargo build --release",
            "tsc --noEmit",
            "git status",
            "ls -la",
            "docker compose up -d",
            "docker compose up --detach",
        ] {
            assert_eq!(classify(cmd), Intent::Neither, "{cmd:?} must stay silent");
        }
    }

    #[test]
    fn hunting_through_log_files_is_recognised() {
        for cmd in [
            "tail -f /tmp/dev.log",
            "tail -100 server.log",
            "grep -i error build.log",
            "cat nohup.out",
            "docker logs web",
            "journalctl -u myapp",
        ] {
            assert_eq!(classify(cmd), Intent::ChasesBug, "{cmd:?} should redirect");
        }
    }

    #[test]
    fn searching_source_is_not_hunting_logs() {
        assert_eq!(classify("grep -rn TODO src/"), Intent::Neither);
        assert_eq!(classify("cat src/main.rs"), Intent::Neither);
    }

    #[test]
    fn a_compound_command_is_judged_on_its_parts() {
        assert_eq!(
            wrapped("cd frontend && npm run dev"),
            "logbear npm run dev",
            "the suggestion must name the server, not the cd"
        );
        assert_eq!(classify("cd frontend && logbear npm run dev"), Intent::Neither);
    }

    #[test]
    fn an_empty_command_is_harmless() {
        assert_eq!(classify(""), Intent::Neither);
        assert_eq!(classify("   "), Intent::Neither);
    }
}

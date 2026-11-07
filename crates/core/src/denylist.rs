//! Whether a command is an interactive session rather than a run, looking through the
//! wrappers people put in front of one.
//!
//! The denylist names programs, but `logbear docker exec -it db psql` has `docker` in
//! `argv[0]` and is a `psql` session all the same. The rule is pure: it reads argv and the
//! settings, and never runs or resolves anything.
//!
//! Flag parsing is a best effort over each tool's common flags. A command it misreads is
//! captured rather than refused, which costs a recorded session and not a broken one. Shell
//! strings (`sh -c 'psql'`) are not looked into.

use crate::proto::Settings;
use std::path::Path;

/// How many wrappers are looked through. Far past anything a person types, and it bounds the
/// work on a command assembled by a script.
const MAX_WRAPPERS: usize = 16;

// Flags that consume the next argument. Anything else that starts with `-` stands alone, so
// a missing entry here reads that flag's value as the next positional.
const ENV_VALUE_FLAGS: &[&str] = &[
    "-u",
    "--unset",
    "-C",
    "--chdir",
    "-S",
    "--split-string",
    "-P",
];
const SUDO_VALUE_FLAGS: &[&str] = &[
    "-C",
    "--close-from",
    "-D",
    "--chdir",
    "-g",
    "--group",
    "-h",
    "--host",
    "-p",
    "--prompt",
    "-R",
    "--chroot",
    "-r",
    "--role",
    "-T",
    "--command-timeout",
    "-t",
    "--type",
    "-U",
    "--other-user",
    "-u",
    "--user",
];
const CONTAINER_GLOBAL_VALUE_FLAGS: &[&str] = &[
    "-c",
    "--context",
    "--connection",
    "--config",
    "-H",
    "--host",
    "-l",
    "--log-level",
    "--root",
    "--runroot",
    "--tlscacert",
    "--tlscert",
    "--tlskey",
    "--url",
];
const CONTAINER_EXEC_VALUE_FLAGS: &[&str] = &[
    "--cidfile",
    "--detach-keys",
    "-e",
    "--env",
    "--env-file",
    "--index",
    "--preserve-fds",
    "-u",
    "--user",
    "-w",
    "--workdir",
];
const COMPOSE_GLOBAL_VALUE_FLAGS: &[&str] = &[
    "--ansi",
    "--env-file",
    "-f",
    "--file",
    "--parallel",
    "--profile",
    "--progress",
    "--project-directory",
    "-p",
    "--project-name",
];
const KUBECTL_VALUE_FLAGS: &[&str] = &[
    "--as",
    "--as-group",
    "--as-uid",
    "--cache-dir",
    "--certificate-authority",
    "--client-certificate",
    "--client-key",
    "--cluster",
    "-c",
    "--container",
    "--context",
    "-f",
    "--filename",
    "--kubeconfig",
    "-n",
    "--namespace",
    "--pod-running-timeout",
    "--request-timeout",
    "-s",
    "--server",
    "--tls-server-name",
    "--token",
    "--user",
    "-v",
    "--v",
];
const SSH_VALUE_FLAGS: &[&str] = &[
    "-B", "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O", "-o", "-P",
    "-p", "-Q", "-R", "-S", "-W", "-w",
];

/// Whether `command` should run untouched because it is, or wraps, a denylisted program.
///
/// Each program in the chain is matched by basename against `settings.denylist`, looking
/// through `env` and `sudo`, `docker` and `podman` `exec` (with `container exec` and `compose
/// exec`), `kubectl exec` and `ssh`. Only the program a wrapper runs is judged: `docker exec
/// psql ls` runs `ls` in a container that happens to be called `psql`.
///
/// `--force` is the caller's to honour.
pub fn is_denied_command(command: &[String], settings: &Settings) -> bool {
    let mut current = command.to_vec();
    for _ in 0..MAX_WRAPPERS {
        let Some(program) = current.first() else {
            return false;
        };
        let name = basename(program);
        if settings.denylist.iter().any(|denied| denied == name) {
            return true;
        }
        match inner_command(name, &current[1..]) {
            Some(inner) => current = inner,
            None => return false,
        }
    }
    false
}

fn basename(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
}

/// What `program` runs, given its arguments, if it is a wrapper and is running something.
fn inner_command(program: &str, args: &[String]) -> Option<Vec<String>> {
    match program {
        "env" => Some(skip_assignments(skip_flags(args, ENV_VALUE_FLAGS)).to_vec()),
        "sudo" => Some(skip_assignments(skip_flags(args, SUDO_VALUE_FLAGS)).to_vec()),
        "docker" | "podman" => container_exec(args),
        "docker-compose" | "podman-compose" => compose_exec(args),
        "kubectl" => kubectl_exec(args),
        "ssh" => ssh_command(args),
        _ => None,
    }
}

fn container_exec(args: &[String]) -> Option<Vec<String>> {
    let (verb, rest) = skip_flags(args, CONTAINER_GLOBAL_VALUE_FLAGS).split_first()?;
    match verb.as_str() {
        "exec" => Some(after_exec_target(rest)),
        "compose" => compose_exec(rest),
        "container" => {
            let (verb, rest) = rest.split_first()?;
            (verb == "exec").then(|| after_exec_target(rest))
        }
        _ => None,
    }
}

fn compose_exec(args: &[String]) -> Option<Vec<String>> {
    let (verb, rest) = skip_flags(args, COMPOSE_GLOBAL_VALUE_FLAGS).split_first()?;
    (verb == "exec").then(|| after_exec_target(rest))
}

/// Past `exec`: flags, the container or service, then the command, whose own flags are its
/// own business.
fn after_exec_target(args: &[String]) -> Vec<String> {
    skip_flags(args, CONTAINER_EXEC_VALUE_FLAGS)
        .split_first()
        .map(|(_target, command)| command.to_vec())
        .unwrap_or_default()
}

fn kubectl_exec(args: &[String]) -> Option<Vec<String>> {
    let (verb, rest) = skip_flags(args, KUBECTL_VALUE_FLAGS).split_first()?;
    if verb != "exec" {
        return None;
    }
    let (_pod, rest) = skip_flags(rest, KUBECTL_VALUE_FLAGS).split_first()?;
    // Flags may follow the pod too, and `--` ends them.
    Some(skip_flags(rest, KUBECTL_VALUE_FLAGS).to_vec())
}

/// The remote command, which ssh joins with spaces and hands to a shell, so one quoted
/// argument and several plain ones are the same thing.
fn ssh_command(args: &[String]) -> Option<Vec<String>> {
    let (_host, rest) = skip_flags(args, SSH_VALUE_FLAGS).split_first()?;
    let remote = skip_flags(rest, SSH_VALUE_FLAGS).join(" ");
    Some(remote.split_whitespace().map(str::to_string).collect())
}

/// The arguments after any leading flags. `--` ends the flags and is dropped with them.
fn skip_flags<'a>(args: &'a [String], value_flags: &[&str]) -> &'a [String] {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if arg == "--" {
            return &args[i + 1..];
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        i += if takes_next_argument(arg, value_flags) {
            2
        } else {
            1
        };
    }
    args.get(i..).unwrap_or_default()
}

/// Whether the flag's value is the next argument, rather than absent or attached.
///
/// `--user=x` and `-ux` carry theirs. In a short cluster like `-itu`, only a value flag in
/// last place reaches for the next argument.
fn takes_next_argument(flag: &str, value_flags: &[&str]) -> bool {
    if flag.starts_with("--") {
        return value_flags.contains(&flag);
    }
    let letters = &flag[1..];
    for (at, letter) in letters.char_indices() {
        if value_flags.contains(&format!("-{letter}").as_str()) {
            return at + letter.len_utf8() == letters.len();
        }
    }
    false
}

/// Past the `NAME=value` pairs `env` and `sudo` take before the command, and `env`'s bare
/// `-`, which means `-i`.
fn skip_assignments(args: &[String]) -> &[String] {
    let is_assignment = |arg: &String| {
        arg.split_once('=')
            .is_some_and(|(name, _)| !name.is_empty() && !name.contains('/'))
    };
    let skipped = args
        .iter()
        .take_while(|a| *a == "-" || is_assignment(a))
        .count();
    &args[skipped..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn denied(line: &str) -> bool {
        is_denied_command(&argv(line), &Settings::default())
    }

    /// The defaults list `ssh` and `sudo` themselves, which would hide everything behind
    /// them. Without those two the wrapper is what gets looked through.
    fn denied_past_wrappers(line: &str) -> bool {
        let mut settings = Settings::default();
        settings.denylist.retain(|d| d != "ssh" && d != "sudo");
        is_denied_command(&argv(line), &settings)
    }

    #[test]
    fn a_denylisted_program_is_denied_by_its_basename() {
        assert!(denied("psql -d app"));
        assert!(denied("/usr/local/bin/vim notes.txt"));
        assert!(!denied("mix phx.server"));
        assert!(!denied("./psql-backup.sh"));
    }

    #[test]
    fn no_command_is_not_denied() {
        assert!(!is_denied_command(&[], &Settings::default()));
    }

    #[test]
    fn the_settings_decide_not_a_built_in_list() {
        let settings = Settings {
            denylist: vec!["mytool".to_string()],
            ..Settings::default()
        };
        assert!(is_denied_command(&argv("docker exec db mytool"), &settings));
        assert!(!is_denied_command(&argv("docker exec db psql"), &settings));
    }

    #[test]
    fn docker_exec_is_judged_by_the_program_inside() {
        assert!(denied("docker exec -it db psql"));
        assert!(denied("docker exec -i -t db psql -U me"));
        assert!(denied("/usr/local/bin/docker exec db /usr/bin/psql"));
        assert!(denied("docker exec -u postgres -w /tmp -e A=1 db psql"));
        assert!(denied("docker exec -itu postgres db psql"));
        assert!(denied("docker exec --user=postgres db psql"));
        assert!(denied("docker exec --user postgres db psql"));
        assert!(denied("docker container exec -it db psql"));
        assert!(denied("docker --context prod exec -it db psql"));
        assert!(!denied("docker exec -it db mix test"));
    }

    #[test]
    fn a_container_named_like_a_denied_program_is_not_the_program() {
        assert!(!denied("docker exec psql ls /tmp"));
    }

    #[test]
    fn other_docker_verbs_are_not_exec() {
        assert!(!denied("docker logs psql"));
        assert!(!denied("docker ps"));
        assert!(!denied("docker build -t vim ."));
        assert!(!denied("docker"));
    }

    #[test]
    fn docker_compose_exec_is_docker_exec_with_a_service() {
        assert!(denied("docker compose exec db psql"));
        assert!(denied("docker compose -f dev.yml exec -T db psql"));
        assert!(denied("docker-compose exec db psql"));
        assert!(!denied("docker compose exec db mix test"));
        assert!(!denied("docker compose up vim"));
    }

    #[test]
    fn podman_exec_is_docker_exec() {
        assert!(denied("podman exec -it db psql"));
        assert!(denied("podman container exec db psql"));
        assert!(!denied("podman exec db ls"));
    }

    #[test]
    fn kubectl_exec_is_judged_by_what_follows_the_pod() {
        assert!(denied("kubectl exec -it web -- psql"));
        assert!(denied("kubectl exec -it web -c app -- psql -U me"));
        assert!(denied("kubectl exec web -c app -it -- psql"));
        assert!(denied("kubectl -n prod exec -it web -- psql"));
        assert!(denied(
            "kubectl --context=prod --namespace prod exec web -- psql"
        ));
        assert!(
            denied("kubectl exec web psql"),
            "the form without `--` is deprecated, not gone"
        );
        assert!(!denied("kubectl exec web -- ls"));
        assert!(!denied("kubectl exec web"));
        assert!(!denied("kubectl get pods"));
        assert!(!denied("kubectl logs psql"));
    }

    #[test]
    fn ssh_is_judged_by_the_remote_command_once_it_is_allowed() {
        assert!(denied("ssh host"), "the default list denies ssh outright");
        assert!(denied_past_wrappers("ssh host psql"));
        assert!(denied_past_wrappers(
            "ssh -t -p 2222 -i key host psql -U me"
        ));
        assert!(denied_past_wrappers(
            "ssh -l me -o BatchMode=yes host vim notes"
        ));
        assert!(denied_past_wrappers("ssh host -- psql"));
        assert!(!denied_past_wrappers("ssh host"));
        assert!(!denied_past_wrappers("ssh host uptime"));
    }

    #[test]
    fn ssh_takes_a_remote_command_as_one_quoted_argument() {
        let mut settings = Settings::default();
        settings.denylist.retain(|d| d != "ssh");
        let command = ["ssh", "host", "psql -U me"].map(str::to_string);
        assert!(is_denied_command(&command, &settings));
    }

    #[test]
    fn sudo_is_judged_by_the_program_it_runs_once_it_is_allowed() {
        assert!(denied("sudo ls"), "the default list denies sudo outright");
        assert!(denied_past_wrappers("sudo psql"));
        assert!(denied_past_wrappers("sudo -u postgres psql"));
        assert!(denied_past_wrappers("sudo -nu postgres psql"));
        assert!(denied_past_wrappers("sudo --user=postgres psql"));
        assert!(denied_past_wrappers("sudo --user postgres psql"));
        assert!(denied_past_wrappers("sudo PGHOST=db psql"));
        assert!(denied_past_wrappers("sudo -- psql"));
        assert!(!denied_past_wrappers("sudo ls"));
        assert!(!denied_past_wrappers("sudo -u postgres"));
    }

    #[test]
    fn env_is_judged_by_the_program_it_runs() {
        assert!(denied("env psql"));
        assert!(denied("env PGHOST=db PGUSER=me psql"));
        assert!(denied("env -i PGHOST=db psql"));
        assert!(denied("env - PGHOST=db psql"));
        assert!(denied("env -u PGHOST psql"));
        assert!(denied("env --unset=PGHOST psql"));
        assert!(denied("env -C /tmp psql"));
        assert!(denied("/usr/bin/env psql"));
        assert!(!denied("env PGHOST=db mix test"));
        assert!(!denied("env"));
        assert!(!denied("env FOO=bar"));
    }

    #[test]
    fn wrappers_nest() {
        assert!(denied("env A=1 docker exec -it db psql"));
        assert!(denied("docker exec -it db env A=1 psql"));
        assert!(denied_past_wrappers(
            "sudo -u me env A=1 docker exec -it db psql"
        ));
        assert!(denied_past_wrappers("ssh host sudo -u postgres psql"));
        assert!(denied_past_wrappers(
            "kubectl exec web -- sudo -u postgres psql"
        ));
    }

    #[test]
    fn a_long_chain_of_wrappers_ends() {
        let chain = "env ".repeat(10_000) + "ls";
        assert!(!denied(&chain));
    }

    #[test]
    fn non_ascii_flags_do_not_panic() {
        assert!(!denied("docker exec -é db ls"));
        assert!(!denied("env -é ls"));
    }
}

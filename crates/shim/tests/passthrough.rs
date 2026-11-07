//! End-to-end passthrough checks against the real binary.
//!
//! Ignored by default, since they spawn `logbear`, which opens a pty and spawns a child:
//!
//! ```text
//! cargo test -p logbear -- --ignored
//! ```
//!
//! Every case runs the daemon-absent path: `LOGBEAR_HOME` and `LOGBEAR_SOCKET` point into a
//! temporary directory, the binary is copied somewhere with no `logbeard` beside it, `PATH`
//! contains only that directory, and `LOGBEAR_DAEMON` names a file that does not exist.
//! Otherwise these tests would silently spawn a real daemon. For the same reason every
//! wrapped command is an absolute path or a `/bin/sh` builtin.

use std::path::Path;
use std::process::Command;

fn logbear(dir: &Path) -> Command {
    let exe = dir.join("logbear");
    if !exe.exists() {
        std::fs::copy(env!("CARGO_BIN_EXE_logbear"), &exe).expect("copy the binary");
    }
    let mut cmd = Command::new(&exe);
    cmd.env("LOGBEAR_HOME", dir);
    cmd.env("LOGBEAR_SOCKET", dir.join("no-daemon.sock"));
    cmd.env("PATH", dir);
    // Naming a daemon that does not exist, rather than unsetting: discovery also looks
    // beside the executable and inside an installed app bundle, so on a developer's own
    // machine an empty PATH is not enough to guarantee the no-daemon path.
    cmd.env("LOGBEAR_DAEMON", dir.join("no-such-logbeard"));
    cmd
}

fn spool_files(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir.join("spool"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

#[test]
#[ignore = "spawns the real binary"]
fn a_command_that_cannot_start_is_not_recorded_as_a_run() {
    let dir = tempfile::tempdir().unwrap();
    // One argument holding a whole command line, which is what a shell that does not
    // word-split a variable hands over. There is no such executable.
    let out = logbear(dir.path()).arg("logs -n 50").output().expect("logbear runs");

    assert_eq!(out.status.code(), Some(127), "shells report a missing command as 127");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("command not found"),
        "stderr was {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        spool_files(dir.path()).is_empty(),
        "a command that never started left a run behind: {:?}",
        spool_files(dir.path())
    );
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_wrapped_command_propagates_its_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let out = logbear(dir.path())
        .args(["/bin/sh", "-c", "exit 7"])
        .output()
        .expect("logbear runs");
    assert_eq!(out.status.code(), Some(7));
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_wrapped_command_reproduces_its_output_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let out = logbear(dir.path())
        .args(["/bin/sh", "-c", "printf hello"])
        .output()
        .expect("logbear runs");
    // Exactly the child's bytes: anything extra means the shim injected something into the
    // pty, such as a stray newline plus EOT on stdin EOF.
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_signalled_child_exits_128_plus_the_signal() {
    let dir = tempfile::tempdir().unwrap();
    let out = logbear(dir.path())
        .args(["/bin/sh", "-c", "kill -TERM $$"])
        .output()
        .expect("logbear runs");
    assert_eq!(out.status.code(), Some(128 + libc::SIGTERM));
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn stdin_reaches_the_child() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let mut child = logbear(dir.path())
        .args(["/bin/sh", "-c", "read line; printf 'got:%s' \"$line\""])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("logbear runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"typed\n")
        .expect("write to stdin");
    let out = child.wait_with_output().expect("logbear finishes");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("got:typed"),
        "stdout was {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_run_with_no_daemon_lands_in_the_spool() {
    let dir = tempfile::tempdir().unwrap();
    let status = logbear(dir.path())
        .args(["/bin/sh", "-c", "printf spooled"])
        .status()
        .expect("logbear runs");
    assert!(status.success());

    let files = spool_files(dir.path());
    assert_eq!(files.len(), 1, "expected exactly one spool file, got {files:?}");

    let bytes = std::fs::read(&files[0]).unwrap();
    let newline = bytes.iter().position(|b| *b == b'\n').expect("a header line");
    let header: serde_json::Value = serde_json::from_slice(&bytes[..newline]).expect("valid header");
    assert_eq!(header["exit_code"], 0, "the exit code was written back into the header");
    assert_eq!(header["stamp"]["command"][0], "/bin/sh");
    assert!(
        bytes.len() > newline + 1,
        "the spool holds a header and no frames"
    );
}

#[test]
#[ignore = "spawns the real binary"]
fn the_no_pty_path_still_propagates_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let out = logbear(dir.path())
        .args(["--no-pty", "/bin/sh", "-c", "printf piped; exit 3"])
        .output()
        .expect("logbear runs");
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "piped");
}

#[test]
#[ignore = "spawns the real binary"]
fn a_denylisted_command_is_exec_ed_with_no_capture() {
    let dir = tempfile::tempdir().unwrap();
    // A stand-in whose basename is on the default denylist, which is what applies with no
    // daemon to ask for settings.
    let fake = dir.path().join("vim");
    std::fs::write(&fake, "#!/bin/sh\nprintf editing\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = logbear(dir.path()).arg("vim").output().expect("logbear runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "editing");
    assert!(
        spool_files(dir.path()).is_empty(),
        "a denylisted command must leave no record at all"
    );
}

#[test]
#[ignore = "spawns the real binary"]
fn force_overrides_the_denylist() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("vim");
    std::fs::write(&fake, "#!/bin/sh\nprintf editing\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = logbear(dir.path())
        .args(["--force", "vim"])
        .output()
        .expect("logbear runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "editing");
    assert_eq!(spool_files(dir.path()).len(), 1, "--force means capture it");
}

#[test]
#[ignore = "spawns the real binary"]
fn subcommands_say_the_daemon_is_down_instead_of_dumping_an_error() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["runs"],
        vec!["logs"],
        vec!["status"],
        vec!["cat", "1"],
        vec!["sql-schema"],
    ] {
        let out = logbear(dir.path()).args(&args).output().expect("logbear runs");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("daemon is not running"),
            "`logbear {}` said {text:?}",
            args.join(" ")
        );
        assert!(
            !text.contains("Error {") && !text.contains("panicked"),
            "`logbear {}` leaked a Rust error: {text:?}",
            args.join(" ")
        );
        assert_ne!(out.status.code(), Some(0), "`logbear {}`", args.join(" "));
    }
}

#[test]
#[ignore = "spawns the real binary"]
fn removing_an_agent_keeps_a_users_hook_and_backs_up_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let settings = home.join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let original = r#"{
  "zebra": 1,
  "hooks": {
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          { "type": "command", "command": "their-tool" },
          { "type": "command", "command": "/somewhere/logbear-hook --harness claude" }
        ]
      }
    ]
  },
  "apple": 2
}
"#;
    std::fs::write(&settings, original).unwrap();

    // Naming an agent keeps the run away from the daemon service and the PATH candidates,
    // some of which are outside any temp HOME.
    let out = logbear(dir.path())
        .env("HOME", &home)
        .env("SHELL", "/bin/sh")
        .env_remove("HERMES_HOME")
        .args(["setup", "--remove", "--agent", "claude", "--yes"])
        .output()
        .expect("logbear runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let after = std::fs::read_to_string(&settings).unwrap();
    assert!(after.contains("their-tool"), "{after}");
    assert!(!after.contains("logbear-hook"), "{after}");
    assert!(after.find("zebra") < after.find("apple"), "{after}");

    let backup = std::fs::read_dir(settings.parent().unwrap())
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().contains("logbear-backup"))
        .expect("a backup beside the config");
    assert_eq!(std::fs::read_to_string(backup.path()).unwrap(), original);
}

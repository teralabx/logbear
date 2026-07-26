//! End-to-end passthrough checks against the real binary.
//!
//! Ignored by default. They spawn `runlog`, which opens a pty and spawns a child, so they
//! are opt-in:
//!
//! ```text
//! cargo test -p runlog -- --ignored
//! ```
//!
//! Every case runs the **daemon-absent** path deliberately, because that is the one that
//! has to work regardless: `RUNLOG_HOME` and `RUNLOG_SOCKET` are redirected into a
//! temporary directory, and the binary is copied somewhere with no `runlogd` beside it and
//! a `PATH` containing only that directory, so the shim's auto-start finds nothing to
//! start. Without that last part these tests would silently spawn a real daemon and stop
//! testing what they claim to. For the same reason every wrapped command is named by
//! absolute path or is a `/bin/sh` builtin.

use std::path::Path;
use std::process::Command;

fn runlog(dir: &Path) -> Command {
    let exe = dir.join("runlog");
    if !exe.exists() {
        std::fs::copy(env!("CARGO_BIN_EXE_runlog"), &exe).expect("copy the binary");
    }
    let mut cmd = Command::new(&exe);
    cmd.env("RUNLOG_HOME", dir);
    cmd.env("RUNLOG_SOCKET", dir.join("no-daemon.sock"));
    cmd.env("PATH", dir);
    cmd.env_remove("RUNLOG_DAEMON");
    cmd
}

fn spool_files(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir.join("spool"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_wrapped_command_propagates_its_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let out = runlog(dir.path())
        .args(["/bin/sh", "-c", "exit 7"])
        .output()
        .expect("runlog runs");
    assert_eq!(out.status.code(), Some(7));
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_wrapped_command_reproduces_its_output_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let out = runlog(dir.path())
        .args(["/bin/sh", "-c", "printf hello"])
        .output()
        .expect("runlog runs");
    // Exactly the child's bytes. Anything extra here means the shim injected something
    // into the pty — the historical failure is a stray newline plus EOT on stdin EOF.
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_signalled_child_exits_128_plus_the_signal() {
    let dir = tempfile::tempdir().unwrap();
    let out = runlog(dir.path())
        .args(["/bin/sh", "-c", "kill -TERM $$"])
        .output()
        .expect("runlog runs");
    assert_eq!(out.status.code(), Some(128 + libc::SIGTERM));
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn stdin_reaches_the_child() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let mut child = runlog(dir.path())
        .args(["/bin/sh", "-c", "read line; printf 'got:%s' \"$line\""])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("runlog runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"typed\n")
        .expect("write to stdin");
    let out = child.wait_with_output().expect("runlog finishes");
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
    let status = runlog(dir.path())
        .args(["/bin/sh", "-c", "printf spooled"])
        .status()
        .expect("runlog runs");
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
    let out = runlog(dir.path())
        .args(["--no-pty", "/bin/sh", "-c", "printf piped; exit 3"])
        .output()
        .expect("runlog runs");
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "piped");
}

#[test]
#[ignore = "spawns the real binary"]
fn a_denylisted_command_is_exec_ed_with_no_capture() {
    let dir = tempfile::tempdir().unwrap();
    // A stand-in for the real thing: what matters is that the basename is on the default
    // denylist, which is what applies when there is no daemon to ask for settings.
    let fake = dir.path().join("vim");
    std::fs::write(&fake, "#!/bin/sh\nprintf editing\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = runlog(dir.path()).arg("vim").output().expect("runlog runs");
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

    let out = runlog(dir.path())
        .args(["--force", "vim"])
        .output()
        .expect("runlog runs");
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
        let out = runlog(dir.path()).args(&args).output().expect("runlog runs");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("daemon is not running"),
            "`runlog {}` said {text:?}",
            args.join(" ")
        );
        assert!(
            !text.contains("Error {") && !text.contains("panicked"),
            "`runlog {}` leaked a Rust error: {text:?}",
            args.join(" ")
        );
        assert_ne!(out.status.code(), Some(0), "`runlog {}`", args.join(" "));
    }
}

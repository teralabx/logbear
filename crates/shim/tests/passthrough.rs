//! End-to-end passthrough checks against the real binary.
//!
//! Ignored by default. They spawn `runlog`, which opens a pty, spawns a child and — if a
//! daemon happens to be listening — registers a run against the developer's real history.
//! That is not something `cargo test` should do on its own, so these are opt-in:
//!
//! ```text
//! cargo test -p runlog -- --ignored
//! ```
//!
//! `RUNLOG_HOME` is redirected to a temporary directory so an opt-in run still cannot
//! write into `~/.runlog`, and `RUNLOG_SOCKET` is pointed at a path nothing is listening
//! on, which exercises the daemon-absent path deliberately — the one that has to work.

use std::process::Command;

fn runlog(dir: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_runlog"));
    cmd.env("RUNLOG_HOME", dir);
    cmd.env("RUNLOG_SOCKET", dir.join("no-daemon.sock"));
    cmd
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_wrapped_command_propagates_its_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let out = runlog(dir.path())
        .args(["sh", "-c", "exit 7"])
        .output()
        .expect("runlog runs");
    assert_eq!(out.status.code(), Some(7));
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_wrapped_command_reproduces_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let out = runlog(dir.path())
        .args(["printf", "hello"])
        .output()
        .expect("runlog runs");
    // A pty turns the child's `\n` into `\r\n` via ONLCR; the payload is what matters.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("hello"),
        "stdout was {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_signalled_child_exits_128_plus_the_signal() {
    let dir = tempfile::tempdir().unwrap();
    let out = runlog(dir.path())
        .args(["sh", "-c", "kill -TERM $$"])
        .output()
        .expect("runlog runs");
    assert_eq!(out.status.code(), Some(128 + libc::SIGTERM));
}

#[test]
#[ignore = "spawns the real binary under a pty"]
fn a_run_with_no_daemon_lands_in_the_spool() {
    let dir = tempfile::tempdir().unwrap();
    let status = runlog(dir.path())
        .args(["printf", "spooled\\n"])
        .status()
        .expect("runlog runs");
    assert!(status.success());

    let spool = dir.path().join("spool");
    let files: Vec<_> = std::fs::read_dir(&spool)
        .expect("the spool directory was created")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(files.len(), 1, "expected exactly one spool file, got {files:?}");

    let bytes = std::fs::read(&files[0]).unwrap();
    let newline = bytes.iter().position(|b| *b == b'\n').expect("a header line");
    let header: serde_json::Value = serde_json::from_slice(&bytes[..newline]).expect("valid header");
    assert_eq!(header["exit_code"], 0);
    assert_eq!(header["stamp"]["command"][0], "printf");
}

#[test]
#[ignore = "spawns the real binary"]
fn the_no_pty_path_still_propagates_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let out = runlog(dir.path())
        .args(["--no-pty", "sh", "-c", "printf piped; exit 3"])
        .output()
        .expect("runlog runs");
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "piped");
}

#[test]
#[ignore = "spawns the real binary"]
fn a_denylisted_command_is_exec_ed_with_no_capture() {
    let dir = tempfile::tempdir().unwrap();
    // `less` is on the default denylist. With no daemon to ask, the default applies, so
    // the command is exec'd directly and nothing is spooled.
    let out = runlog(dir.path())
        .args(["less", "--version"])
        .output()
        .expect("runlog runs");
    assert!(out.status.success(), "less --version should succeed");

    let spooled = std::fs::read_dir(dir.path().join("spool"))
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert_eq!(spooled, 0, "a denylisted command must leave no record");
}

//! The shim against a real daemon: the cases where the daemon falls behind or goes away.
//!
//! Ignored by default because they need `logbeard` built and take seconds. Each case has its
//! own short `LOGBEAR_HOME`, since a unix socket path is limited to about 104 bytes.
//!
//! ```text
//! cargo build --bins && cargo test -p logbear --test daemon -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Tests in one process run in parallel, so each needs a home of its own.
static NEXT_HOME: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

struct Home {
    dir: PathBuf,
    daemon: Option<Child>,
}

fn daemon_binary() -> PathBuf {
    let shim = PathBuf::from(env!("CARGO_BIN_EXE_logbear"));
    shim.with_file_name("logbeard")
}

impl Home {
    fn new() -> Home {
        let n = NEXT_HOME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/lbt{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut home = Home { dir, daemon: None };
        home.start_daemon();
        home
    }

    fn start_daemon(&mut self) {
        let child = Command::new(daemon_binary())
            .arg("serve")
            .env("LOGBEAR_HOME", &self.dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start logbeard: run `cargo build --bins` first");
        self.daemon = Some(child);
        let socket = self.dir.join("logbeard.sock");
        wait_until("the daemon to listen", || socket.exists());
    }

    /// A clean stop, as an upgrade would do.
    fn stop_daemon(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let _ = child.wait();
        }
    }

    fn shim(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_logbear"));
        cmd.env("LOGBEAR_HOME", &self.dir);
        cmd.env("LOGBEAR_DAEMON", daemon_binary());
        cmd.current_dir(&self.dir);
        cmd
    }

    fn logs(&self, args: &[&str]) -> String {
        let out = self.shim().arg("logs").args(args).output().expect("logbear logs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn query(&self, sql: &str) -> String {
        let out = self.shim().args(["query", sql]).output().expect("logbear query");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.stop_daemon();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn script(dir: &Path, body: &str) -> String {
    let path = dir.join("emit.sh");
    std::fs::write(&path, body).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
#[ignore = "needs logbeard built"]
fn an_error_after_a_burst_is_not_lost() {
    let home = Home::new();
    // 20 MB as fast as the pipe takes it, then the line that matters and a failing exit.
    let emit = script(
        &home.dir,
        "i=0; while [ $i -lt 400 ]; do /bin/dd if=/dev/zero bs=50000 count=1 2>/dev/null \
         | /usr/bin/tr '\\0' 'x'; echo; i=$((i+1)); done; \
         echo '[error] the line after the burst'; exit 7\n",
    );
    let status = home
        .shim()
        .args(["/bin/sh", &emit])
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(7));

    wait_until("the error to be stored", || {
        home.logs(&["--errors"]).contains("the line after the burst")
    });
    wait_until("the exit code to be recorded", || {
        home.query("SELECT exit_code FROM runs").contains('7')
    });
    let runs = home.query("SELECT id FROM runs");
    assert!(runs.contains("1 row"), "the burst split the run: {runs}");
}

#[test]
#[ignore = "needs logbeard built"]
fn a_daemon_restart_mid_run_keeps_the_run_and_its_output_in_order() {
    let mut home = Home::new();
    let emit = script(
        &home.dir,
        "echo before-restart; /bin/sleep 2; echo during-restart; /bin/sleep 3; \
         echo '[error] after-restart'; exit 3\n",
    );
    let mut child = home
        .shim()
        .args(["/bin/sh", &emit])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();

    // Stored, so what follows is a restart in the middle of the run and not during its start.
    wait_until("the first line", || home.logs(&[]).contains("before-restart"));
    home.stop_daemon();
    std::thread::sleep(Duration::from_millis(2500));
    home.start_daemon();

    assert_eq!(child.wait().unwrap().code(), Some(3));
    wait_until("the last line", || home.logs(&[]).contains("after-restart"));
    let logs = home.logs(&[]);
    let at = |needle: &str| logs.find(needle).unwrap_or_else(|| panic!("{needle} missing: {logs}"));
    assert!(at("before-restart") < at("during-restart"));
    assert!(at("during-restart") < at("after-restart"));
    wait_until("the exit code", || home.query("SELECT exit_code FROM runs").contains('3'));
}

#[test]
#[ignore = "needs logbeard built"]
fn a_run_that_never_reached_the_daemon_appears_without_another_command() {
    let mut home = Home::new();
    home.stop_daemon();
    let mut cmd = home.shim();
    // No daemon to start, so the run can only go to the spool.
    cmd.env("LOGBEAR_DAEMON", home.dir.join("none"));
    let status = cmd.args(["/bin/sh", "-c", "echo '[error] spooled alone'; exit 2"]).status().unwrap();
    assert_eq!(status.code(), Some(2));

    home.start_daemon();
    wait_until("the sweep", || home.logs(&[]).contains("spooled alone"));
}

#[test]
#[ignore = "needs logbeard built"]
fn resizes_do_not_stall_the_stream() {
    let home = Home::new();
    let emit = script(
        &home.dir,
        "i=0; while [ $i -lt 150 ]; do echo line-$i; /bin/sleep 0.01; i=$((i+1)); done\n",
    );
    let mut child = home.shim().args(["/bin/sh", &emit]).stdout(Stdio::null()).spawn().unwrap();
    let pid = child.id() as i32;
    for _ in 0..2000 {
        unsafe { libc::kill(pid, libc::SIGWINCH) };
        std::thread::sleep(Duration::from_micros(500));
    }
    child.wait().unwrap();
    wait_until("every line", || home.logs(&["-n", "1000"]).contains("line-149"));
}

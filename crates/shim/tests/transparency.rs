//! `logbear <cmd>` must behave like `<cmd>`, whatever its standard streams are attached to.
//!
//! Each case compares against what the bare command does: same bytes, same exit code, and
//! no hang. Every run is bounded, so a regression fails here rather than stalling the suite.
//!
//! Agents run commands with no terminal at all, so the pipe, file and `/dev/null` shapes are
//! the ones that matter most; those run by default. Cases that need a real terminal open
//! their own pty.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const BOUND: Duration = Duration::from_secs(10);

fn logbear(dir: &Path) -> Command {
    let exe = dir.join("logbear");
    if !exe.exists() {
        std::fs::copy(env!("CARGO_BIN_EXE_logbear"), &exe).expect("copy the binary");
    }
    let mut cmd = Command::new(&exe);
    cmd.env("LOGBEAR_HOME", dir);
    cmd.env("LOGBEAR_SOCKET", dir.join("no-daemon.sock"));
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("LOGBEAR_DAEMON", dir.join("no-such-logbeard"));
    cmd
}

struct Finished {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    took: Duration,
}

/// Runs `cmd` to completion, killing it and failing the test if it outlives [`BOUND`].
fn bounded(mut cmd: Command, stdin: Option<&[u8]>) -> Finished {
    let started = Instant::now();
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else if cfg!(unix) {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.process_group(0);
    let mut child = cmd.spawn().expect("spawn");
    if let Some(bytes) = stdin {
        let mut input = child.stdin.take().unwrap();
        let bytes = bytes.to_vec();
        std::thread::spawn(move || {
            let _ = input.write_all(&bytes);
        });
    }
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });
    let err_reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err.read_to_end(&mut v);
        v
    });
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            let took = started.elapsed();
            return Finished {
                status,
                stdout: out_reader.join().unwrap(),
                stderr: err_reader.join().unwrap(),
                took,
            };
        }
        if started.elapsed() > BOUND {
            unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
            panic!("hung for {BOUND:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `sh -c <script>` with `$LOGBEAR` naming the binary, for the shapes that need a shell to
/// build a pipeline around it.
fn shell(dir: &Path, script: &str) -> Finished {
    let exe = logbear(dir).get_program().to_owned();
    let mut cmd = logbear(dir);
    let envs: Vec<_> = cmd
        .get_envs()
        .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
        .collect();
    cmd = Command::new("/bin/sh");
    for (k, v) in envs {
        if let Some(v) = v {
            cmd.env(k, v);
        }
    }
    cmd.env("LOGBEAR", exe).args(["-c", script]);
    bounded(cmd, None)
}

fn spools(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir.join("spool"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

fn spooled_text(dir: &Path) -> String {
    let mut all = Vec::new();
    for path in spools(dir) {
        all.extend(std::fs::read(path).unwrap_or_default());
    }
    String::from_utf8_lossy(&all).into_owned()
}

#[test]
fn a_closed_downstream_pipe_ends_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let done = shell(dir.path(), r#""$LOGBEAR" /usr/bin/yes | /usr/bin/head -1"#);
    assert_eq!(done.stdout, b"y\n");
    assert!(done.status.success());
}

#[test]
fn a_program_that_reads_stdin_from_dev_null_sees_eof() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = logbear(dir.path());
    cmd.args(["/usr/bin/wc", "-l"]);
    let done = bounded(cmd, None);
    assert_eq!(String::from_utf8_lossy(&done.stdout).trim(), "0");
}

#[test]
fn piped_stdin_reaches_the_child_and_closes() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = logbear(dir.path());
    cmd.args(["/bin/sh", "-c", "exec /bin/cat"]);
    let done = bounded(cmd, Some(b"hello"));
    assert_eq!(done.stdout, b"hello");
    assert_eq!(done.status.code(), Some(0));
}

#[test]
fn stdin_is_passed_through_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    // A line over a pty's canonical limit, and bytes a line discipline would interpret.
    let mut input = vec![b'x'; 5 * 1024];
    input.extend_from_slice(&[0x03, 0x04, 0x1a, 0x1c, b'\n', 0x00, 0xff]);
    let mut cmd = logbear(dir.path());
    cmd.arg("/bin/cat");
    let done = bounded(cmd, Some(&input));
    assert_eq!(done.stdout, input);
    assert_eq!(done.status.code(), Some(0));
}

#[test]
fn redirected_output_keeps_its_exact_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = logbear(dir.path());
    cmd.args(["/usr/bin/printf", "a\\n"]);
    assert_eq!(bounded(cmd, None).stdout, b"a\n");
}

#[test]
fn binary_output_survives() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = logbear(dir.path());
    cmd.args(["/usr/bin/printf", "\\000\\001\\377\\r\\n\\033[0m"]);
    assert_eq!(bounded(cmd, None).stdout, b"\x00\x01\xff\r\n\x1b[0m");
}

#[test]
fn a_program_can_tell_that_its_output_is_a_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = logbear(dir.path());
    cmd.args([
        "/bin/sh",
        "-c",
        "[ -t 0 ] && echo in-tty; [ -t 1 ] && echo out-tty; [ -t 2 ] && echo err-tty; echo end",
    ]);
    assert_eq!(bounded(cmd, None).stdout, b"end\n");
}

#[test]
fn stdout_and_stderr_stay_separate() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = logbear(dir.path());
    cmd.args(["/bin/sh", "-c", "echo out; echo err >&2"]);
    let done = bounded(cmd, None);
    assert_eq!(done.stdout, b"out\n");
    assert_eq!(done.stderr, b"err\n");
}

#[test]
fn stdout_and_stderr_on_one_pipe_keep_their_order() {
    let dir = tempfile::tempdir().unwrap();
    let done = shell(
        dir.path(),
        r#""$LOGBEAR" /bin/sh -c 'for i in 1 2 3 4 5 6; do echo o$i; echo e$i >&2; done' 2>&1"#,
    );
    assert_eq!(
        String::from_utf8_lossy(&done.stdout),
        "o1\ne1\no2\ne2\no3\ne3\no4\ne4\no5\ne5\no6\ne6\n"
    );
}

#[test]
fn both_streams_are_captured() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = logbear(dir.path());
    cmd.args(["/bin/sh", "-c", "echo captured-out; echo captured-err >&2"]);
    bounded(cmd, None);
    let text = spooled_text(dir.path());
    assert!(text.contains("captured-out"), "{text:?}");
    assert!(text.contains("captured-err"), "{text:?}");
}

#[test]
fn a_child_killed_by_sigpipe_reports_it_the_way_a_shell_does() {
    let dir = tempfile::tempdir().unwrap();
    let done = shell(
        dir.path(),
        r#""$LOGBEAR" /usr/bin/yes | /usr/bin/true; :; "$LOGBEAR" /bin/sh -c 'exit 9'; echo $?"#,
    );
    assert_eq!(done.stdout, b"9\n");
}

#[test]
fn a_background_grandchild_does_not_hold_the_command_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = logbear(dir.path());
    cmd.args(["/bin/sh", "-c", "/bin/sleep 30 & exit 3"]);
    let done = bounded(cmd, None);
    assert_eq!(done.status.code(), Some(3));
    assert!(done.took < Duration::from_secs(3), "took {:?}", done.took);
}

#[test]
fn a_missing_daemon_costs_almost_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = logbear(dir.path());
    cmd.args(["/bin/sh", "-c", "exit 4"]);
    let done = bounded(cmd, None);
    assert_eq!(done.status.code(), Some(4));
    assert!(done.took < Duration::from_millis(1000), "took {:?}", done.took);
}

#[test]
fn a_wedged_daemon_cannot_stop_the_child_starting() {
    let dir = tempfile::tempdir().unwrap();
    // Accepts connections in the kernel and never answers one.
    let socket = dir.path().join("wedged.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let marker = dir.path().join("started");

    let mut cmd = logbear(dir.path());
    cmd.env("LOGBEAR_SOCKET", &socket);
    cmd.args(["/bin/sh", "-c", &format!(": > {}; echo wedged-output; exit 6", marker.display())]);
    let done = bounded(cmd, None);

    assert!(marker.exists());
    assert_eq!(done.stdout, b"wedged-output\n");
    assert_eq!(done.status.code(), Some(6));
    assert!(done.took < Duration::from_secs(3), "took {:?}", done.took);
    assert!(spooled_text(dir.path()).contains("wedged-output"), "the output was lost");
}

#[test]
fn a_private_session_inside_a_wrapper_is_not_captured() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let docker = dir.path().join("docker");
    std::fs::write(&docker, "#!/bin/sh\necho session-output\n").unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut cmd = logbear(dir.path());
    cmd.arg(&docker).args(["exec", "-it", "db", "psql"]);
    let done = bounded(cmd, None);

    assert_eq!(done.stdout, b"session-output\n");
    assert!(spools(dir.path()).is_empty(), "a database session was recorded");
}

#[test]
fn a_spool_is_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = logbear(dir.path());
    cmd.args(["/bin/sh", "-c", "echo x"]);
    bounded(cmd, None);
    let files = spools(dir.path());
    assert!(!files.is_empty());
    for file in files {
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", file.display());
    }
}

// ---------------------------------------------------------------------------
// Real terminals
// ---------------------------------------------------------------------------

struct Terminal {
    master: std::fs::File,
    slave: OwnedFd,
}

fn terminal() -> Terminal {
    let (mut m, mut s) = (0, 0);
    let mut ws = libc::winsize { ws_row: 40, ws_col: 120, ws_xpixel: 0, ws_ypixel: 0 };
    let rc = unsafe {
        libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws)
    };
    assert_eq!(rc, 0);
    unsafe {
        Terminal {
            master: std::fs::File::from_raw_fd(m),
            slave: OwnedFd::from_raw_fd(s),
        }
    }
}

/// Starts `cmd` as a session leader whose controlling terminal is `term`.
fn on_terminal(cmd: &mut Command, term: &Terminal, streams: [bool; 3]) -> std::process::Child {
    let stdio = |on: bool| {
        if on {
            Stdio::from(term.slave.try_clone().unwrap())
        } else {
            Stdio::null()
        }
    };
    cmd.stdin(stdio(streams[0]));
    cmd.stdout(stdio(streams[1]));
    cmd.stderr(stdio(streams[2]));
    let fd = term.slave.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            libc::ioctl(fd, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    cmd.spawn().expect("spawn on a terminal")
}

/// Reads the terminal until `want` appears.
fn expect(master: &mut std::fs::File, want: &str) -> String {
    let fd = master.as_raw_fd();
    let started = Instant::now();
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if String::from_utf8_lossy(&seen).contains(want) {
            return String::from_utf8_lossy(&seen).into_owned();
        }
        assert!(
            started.elapsed() < BOUND,
            "never saw {want:?} in {:?}",
            String::from_utf8_lossy(&seen)
        );
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, 100) } > 0 {
            match master.read(&mut buf) {
                Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

fn wait_bounded(child: &mut std::process::Child) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > BOUND {
            let _ = child.kill();
            panic!("hung on a terminal");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_program_on_a_terminal_sees_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let mut term = terminal();
    let mut cmd = logbear(dir.path());
    cmd.args([
        "/bin/sh",
        "-c",
        "[ -t 0 ] && [ -t 1 ] && [ -t 2 ] && echo all-ttys; /bin/stty size",
    ]);
    let mut child = on_terminal(&mut cmd, &term, [true, true, true]);
    let seen = expect(&mut term.master, "120");
    assert!(seen.contains("all-ttys"), "{seen:?}");
    assert!(wait_bounded(&mut child).success());
}

#[test]
fn a_redirected_stdin_keeps_a_terminal_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let mut term = terminal();
    let mut cmd = logbear(dir.path());
    cmd.args(["/bin/sh", "-c", "[ -t 0 ] || echo in-not; [ -t 1 ] && echo out-tty"]);
    let mut child = on_terminal(&mut cmd, &term, [false, true, true]);
    let seen = expect(&mut term.master, "out-tty");
    assert!(seen.contains("in-not"), "{seen:?}");
    assert!(wait_bounded(&mut child).success());
}

#[test]
fn ctrl_c_reaches_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let mut term = terminal();
    let mut cmd = logbear(dir.path());
    cmd.args(["/bin/sh", "-c", "echo ready; exec /bin/sleep 30"]);
    let mut child = on_terminal(&mut cmd, &term, [true, true, true]);
    expect(&mut term.master, "ready");
    std::thread::sleep(Duration::from_millis(200));
    term.master.write_all(&[0x03]).unwrap();
    // Reads what the terminal is sent, as a real one would: the kernel holds a process's
    // exit until its terminal has taken the output, here the echoed `^C`.
    let fd = term.master.as_raw_fd();
    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
    let mut sink = [0u8; 256];
    let started = Instant::now();
    let status = loop {
        let _ = term.master.read(&mut sink);
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < BOUND, "Ctrl-C did not end the command");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(status.code(), Some(130));
}

#[test]
fn ctrl_z_stops_the_wrapper_and_the_child_and_both_resume() {
    use std::os::unix::process::ExitStatusExt;
    let dir = tempfile::tempdir().unwrap();
    let mut term = terminal();
    let mut cmd = logbear(dir.path());
    // Prints, waits for a line, prints again: proves the child really did resume.
    cmd.args(["/bin/sh", "-c", "echo before; read line; echo after-$line"]);
    let mut child = on_terminal(&mut cmd, &term, [true, true, true]);
    let pid = child.id() as i32;
    expect(&mut term.master, "before");
    std::thread::sleep(Duration::from_millis(200));
    term.master.write_all(&[0x1a]).unwrap();

    let started = Instant::now();
    let mut status = 0;
    loop {
        let got = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if got == pid {
            break;
        }
        assert!(started.elapsed() < BOUND, "Ctrl-Z did not stop the wrapper");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(libc::WIFSTOPPED(status), "status {status:#x}");

    unsafe { libc::kill(pid, libc::SIGCONT) };
    std::thread::sleep(Duration::from_millis(200));
    term.master.write_all(b"ok\n").unwrap();
    expect(&mut term.master, "after-ok");
    let status = wait_bounded(&mut child);
    assert_eq!(status.code(), Some(0), "signal {:?}", status.signal());
}

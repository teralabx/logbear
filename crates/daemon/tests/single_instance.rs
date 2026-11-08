//! The daemon owns every project database, so a second one is not a slow path — it is two
//! writers on the same files. These tests pin the guarantee that only one can ever serve.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A daemon that dies with the test, including when the test dies by panicking. Without
/// this, a failing assertion leaks a real daemon onto the developer's machine — which is
/// precisely the mess these tests exist to prevent.
struct Daemon(Child);

impl Daemon {
    fn start(home: &Path) -> Daemon {
        Daemon(
            Command::new(env!("CARGO_BIN_EXE_logbeard"))
                .arg("serve")
                .env("LOGBEAR_HOME", home)
                .env_remove("LOGBEAR_SOCKET")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("logbeard should start"),
        )
    }

    fn running(&mut self) -> bool {
        self.0.try_wait().expect("wait should succeed").is_none()
    }

    /// Waits for a process that is expected to bow out, returning its exit code and stderr.
    fn wait_for_exit(&mut self, what: &str) -> (Option<i32>, String) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(status) = self.0.try_wait().expect("wait should succeed") {
                let mut err = String::new();
                if let Some(mut pipe) = self.0.stderr.take() {
                    let _ = pipe.read_to_string(&mut err);
                }
                return (status.code(), err);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("{what} was still running after 10s; a second daemon is serving");
    }

    fn pid(&self) -> i32 {
        self.0.id() as i32
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Blocks until the daemon answers, so the test never races its startup.
fn wait_until_serving(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("daemon never started listening on {}", socket.display());
}

/// The regression: the socket file was the only thing standing between one daemon and two.
/// Anything that removes it — a cleanup, a restore, an unclean kill of a *different* daemon —
/// let the next `serve` bind a second listener while the first was still live.
#[test]
fn a_second_daemon_refuses_to_start_even_when_the_socket_file_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let socket = home.join("logbeard.sock");

    let mut first = Daemon::start(home);
    wait_until_serving(&socket);

    // The daemon is alive and serving; only its socket file has gone missing.
    std::fs::remove_file(&socket).expect("socket should be removable");

    let mut second = Daemon::start(home);
    let (code, err) = second.wait_for_exit("the second daemon");

    assert_eq!(code, Some(0), "a duplicate is not a failure, it is a no-op");
    assert!(
        err.contains("already running"),
        "it should say why it stood down, got: {err}"
    );
    assert!(
        first.running(),
        "the original daemon must survive an attempted takeover"
    );
}

/// Two daemons racing from cold start: whoever loses must not leave a half-owned socket
/// or take the winner's files with it when it exits.
#[test]
fn racing_daemons_settle_on_exactly_one_listener() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let socket = home.join("logbeard.sock");

    let mut daemons: Vec<Daemon> = (0..4).map(|_| Daemon::start(home)).collect();
    wait_until_serving(&socket);

    // Give every loser time to notice and stand down.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut alive = daemons.len();
    while Instant::now() < deadline {
        alive = daemons.iter_mut().fold(0, |n, d| n + d.running() as usize);
        if alive == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(alive, 1, "exactly one daemon should still be running");

    // The survivor still owns a working socket: the losers cleaned up nothing of its.
    std::os::unix::net::UnixStream::connect(&socket)
        .expect("the surviving daemon should still be reachable");
}

/// A terminated daemon must close its own door: SIGTERM is how launchd, systemd and logout
/// stop it, and the old code left a socket behind that the next start had to guess about.
#[test]
fn sigterm_makes_the_daemon_clean_up_after_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let socket = home.join("logbeard.sock");

    let mut daemon = Daemon::start(home);
    wait_until_serving(&socket);

    unsafe { libc::kill(daemon.pid(), libc::SIGTERM) };
    let (code, _) = daemon.wait_for_exit("the signalled daemon");

    assert_eq!(code, Some(0), "SIGTERM is a clean stop, not a crash");
    assert!(
        !socket.exists(),
        "the socket must not outlive the daemon that bound it"
    );
}

/// `brew upgrade` swaps the binary under a running daemon. It has to step aside on its own,
/// with a code launchd treats as "restart me", or the old version serves until a reboot.
#[test]
fn a_replaced_daemon_exits_for_its_successor() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let exe = dir.path().join("logbeard");
    std::fs::copy(env!("CARGO_BIN_EXE_logbeard"), &exe).unwrap();

    let mut cmd = Command::new(&exe);
    cmd.arg("serve")
        .env("LOGBEAR_HOME", &home)
        .env_remove("LOGBEAR_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A file copied a moment ago can fail to run with `ETXTBSY` on Linux: another test that
    // forks while the copy is open for writing hands it to its child until that child execs.
    // The window is short, so a brief retry gets past it. macOS has no such error.
    let deadline = Instant::now() + Duration::from_secs(2);
    let child = loop {
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            other => break other.expect("logbeard should start"),
        }
    };
    let mut daemon = Daemon(child);
    wait_until_serving(&home.join("logbeard.sock"));

    // install.sh's way: remove, then copy a new file to the same path.
    std::fs::remove_file(&exe).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_logbeard"), &exe).unwrap();

    let (code, _) = daemon.wait_for_exit("the replaced daemon");
    assert_eq!(code, Some(75));
    assert!(!home.join("logbeard.sock").exists(), "socket left behind");
}

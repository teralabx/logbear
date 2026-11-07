//! What the daemon does when the ground moves under it. Each of these used to end in a dead
//! or immortal process rather than a failed assertion, so they run the real binary.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon(Child);

impl Daemon {
    fn start(home: &Path) -> Daemon {
        Daemon(
            Command::new(env!("CARGO_BIN_EXE_logbeard"))
                .arg("serve")
                .env("LOGBEAR_HOME", home)
                .env_remove("LOGBEAR_SOCKET")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("logbeard should start"),
        )
    }

    fn wait_for_exit(&mut self, within: Duration) -> Option<i32> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(status) = self.0.try_wait().expect("wait should succeed") {
                return Some(status.code().unwrap_or(-1));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn running(&mut self) -> bool {
        self.0.try_wait().expect("wait should succeed").is_none()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

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

/// `brew test` and every test suite point the daemon at a temporary home and then delete
/// it, leaving a process with no socket that nothing can reach or stop.
#[test]
fn a_daemon_exits_when_its_temporary_home_disappears() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("h");
    std::fs::create_dir_all(&home).unwrap();
    let mut daemon = Daemon::start(&home);
    wait_until_serving(&home.join("logbeard.sock"));

    std::fs::remove_dir_all(&home).unwrap();

    assert_eq!(
        daemon.wait_for_exit(Duration::from_secs(10)),
        Some(0),
        "the daemon outlived its home"
    );
}

#[test]
fn a_home_replaced_at_the_same_path_does_not_keep_the_daemon_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("h");
    std::fs::create_dir_all(&home).unwrap();
    let mut daemon = Daemon::start(&home);
    wait_until_serving(&home.join("logbeard.sock"));

    std::fs::remove_dir_all(&home).unwrap();
    std::fs::create_dir_all(&home).unwrap();

    assert!(daemon.wait_for_exit(Duration::from_secs(10)).is_some());
}

#[test]
fn a_damaged_index_does_not_stop_the_daemon_starting() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::write(home.join("index.db"), b"not a database in the slightest").unwrap();

    let mut daemon = Daemon::start(home);
    wait_until_serving(&home.join("logbeard.sock"));

    assert!(daemon.running(), "a restart loop under launchd starts with an exit here");
    let aside = std::fs::read_dir(home)
        .unwrap()
        .filter(|f| f.as_ref().unwrap().file_name().to_string_lossy().contains(".corrupt-"))
        .count();
    assert_eq!(aside, 1, "the damaged index should be kept");
}

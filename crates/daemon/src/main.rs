//! `logbeard` — the long-lived process that owns the databases.
//!
//! It outlives the desktop app and auto-starts on the first `logbear` invocation.
//! Everything it does happens behind one unix socket.

mod conn;
mod lock;
mod os_notify;
mod spool;
mod state;

use clap::{Parser, Subcommand};
use lock::InstanceLock;
use logbear_core::client::Client;
use logbear_core::paths;
use logbear_core::proto::Request;
use state::Daemon;
use std::os::unix::net::UnixListener;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// How often live runs are ticked so a quiet server's last entry becomes visible.
const TICK: Duration = Duration::from_millis(200);
/// How often retention is enforced across every open project.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(name = "logbeard", version, about = "The logbear daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run in the foreground. This is what the launch agent and auto-start invoke.
    Serve,
    /// Ask a running daemon to exit.
    Stop,
    /// Print the running daemon's status as JSON.
    Status,
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command.unwrap_or(Cmd::Serve) {
        Cmd::Serve => serve(),
        Cmd::Stop => stop(),
        Cmd::Status => status(),
    };
    std::process::exit(code);
}

fn serve() -> i32 {
    if let Err(e) = paths::ensure_home() {
        eprintln!("logbeard: cannot create {}: {e}", paths::home().display());
        return 1;
    }

    // Uniqueness is settled before anything is touched, and by the kernel rather than by
    // inspecting files: everything below assumes no other daemon is alive, and the lock is
    // what makes that true.
    let lock_path = paths::pid_file();
    let lock = match InstanceLock::acquire(&lock_path) {
        Ok(Some(l)) => l,
        Ok(None) => {
            match InstanceLock::holder(&lock_path) {
                Some(pid) => eprintln!("logbeard: already running (pid {pid})"),
                None => eprintln!("logbeard: already running"),
            }
            return 0;
        }
        Err(e) => {
            eprintln!("logbeard: cannot lock {}: {e}", lock_path.display());
            return 1;
        }
    };

    // Holding the lock proves any socket still on disk was left by a daemon that is gone,
    // so reclaiming it cannot strand a live one.
    let socket = paths::socket();
    let _ = std::fs::remove_file(&socket);

    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("logbeard: cannot bind {}: {e}", socket.display());
            return 1;
        }
    };
    // The socket is a private channel to this user's entire log history.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    }

    let daemon = match Daemon::open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("logbeard: cannot open storage: {e}");
            let _ = std::fs::remove_file(&socket);
            return 1;
        }
    };

    // Registered only now: before the socket exists there is nothing to tear down, and a
    // signal arriving mid-startup would leave the handler describing a daemon that never ran.
    spawn_signal_handler(daemon.clone());

    log(&format!(
        "logbeard {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        socket.display()
    ));

    // Before the socket accepts anything: a run whose daemon died is not running, whatever
    // the database still says.
    let orphans = daemon.reconcile_orphaned_runs();
    if orphans > 0 {
        log(&format!(
            "closed {orphans} run(s) left open by a previous daemon"
        ));
    }

    let drained = spool::drain_all(&daemon);
    if drained > 0 {
        log(&format!("backfilled {drained} spooled run(s)"));
    }

    spawn_maintenance(daemon.clone());

    // A blocking accept would wedge the process after a shutdown request, since nothing
    // else would connect to wake it.
    let _ = listener.set_nonblocking(true);
    accept_loop(&listener, &daemon);

    daemon.close_active_runs();
    // Ours to remove: nothing else could have bound it while the lock was held.
    let _ = std::fs::remove_file(&socket);
    log("logbeard stopped");
    // Last, so the door stays shut until the socket is gone: a daemon starting the instant
    // this one lets go must not find a socket file with no listener behind it.
    lock.release();
    0
}

/// Turns the signals that stop a daemon into the shutdown it already knows how to do.
///
/// Without this, `launchctl bootout`, `systemctl stop` and logout all kill the process
/// outright, leaving the socket on disk and every live run open in the database.
fn spawn_signal_handler(daemon: Arc<Daemon>) {
    use signal_hook::consts::signal::*;
    use signal_hook::iterator::Signals;

    let Ok(mut signals) = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT]) else {
        log("could not install signal handlers; a stop will not be clean");
        return;
    };
    std::thread::Builder::new()
        .name("logbeard-signals".into())
        .spawn(move || {
            if let Some(sig) = signals.forever().next() {
                log(&format!("signal {sig}; shutting down"));
                daemon.shutdown.store(true, Ordering::SeqCst);
            }
        })
        .ok();
}

fn accept_loop(listener: &UnixListener, daemon: &Arc<Daemon>) {
    while !daemon.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let d = daemon.clone();
                std::thread::spawn(move || conn::serve(d, stream));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                log(&format!("accept failed: {e}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// One thread for the periodic work: ticking live runs and enforcing retention.
fn spawn_maintenance(daemon: Arc<Daemon>) {
    std::thread::spawn(move || {
        let mut last_sweep = std::time::Instant::now();
        while !daemon.shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(TICK);
            daemon.tick_runs();

            if last_sweep.elapsed() >= SWEEP_EVERY {
                last_sweep = std::time::Instant::now();
                for handle in daemon.open_projects() {
                    let cap = daemon.retention_cap(&handle.project);
                    match logbear_core::store::retention::enforce(&handle.db, cap) {
                        Ok(n) if n > 0 => log(&format!(
                            "{}: evicted the raw log of {n} old run(s) to stay under {} bytes",
                            handle.project.name, cap
                        )),
                        Err(e) => log(&format!("{}: retention failed: {e}", handle.project.name)),
                        _ => {}
                    }
                }
            }
        }
    });
}

fn stop() -> i32 {
    match Client::connect() {
        Ok(mut c) => {
            let _ = c.call::<serde_json::Value>(&Request::Shutdown);
            println!("logbeard: shutdown requested");
            0
        }
        Err(_) => {
            println!("logbeard: not running");
            0
        }
    }
}

fn status() -> i32 {
    match Client::connect().and_then(|mut c| c.status()) {
        Ok(s) => {
            println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("logbeard: {e}");
            1
        }
    }
}

/// Timestamped stderr; whatever starts the daemon redirects it to `~/.logbear/logbeard.log`.
fn log(msg: &str) {
    let now = time::OffsetDateTime::now_utc();
    eprintln!(
        "[{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z] {msg}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
}

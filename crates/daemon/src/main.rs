//! `logbeard` — the long-lived process that owns the databases.
//!
//! It outlives the desktop app and auto-starts on the first `logbear` invocation.
//! Everything it does happens behind one unix socket.

mod conn;
mod lock;
mod os_notify;
mod retire;
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
/// How often the spool directory is checked for runs that could not reach the daemon.
const SPOOL_SWEEP: Duration = Duration::from_secs(1);
/// How long a stopping daemon keeps reading what is already in its sockets.
const SHUTDOWN_DRAIN: Duration = Duration::from_millis(150);
/// How often a temporary home is checked for still being there.
const HOME_CHECK_EVERY: Duration = Duration::from_secs(1);
/// How often open projects are checked against their caps. Often, since a check is a few
/// `stat` calls and a firehose can write gigabytes a minute.
const SWEEP_EVERY: Duration = Duration::from_secs(5);
/// How often idle projects are closed and idle memory handed back.
const IDLE_EVERY: Duration = Duration::from_secs(60);
/// The file descriptor limit asked for. launchd starts a job with 256. macOS refuses more
/// than this even when the hard limit says unlimited.
const WANTED_FDS: u64 = 10_240;

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
    // First, so nothing below can create a file any wider. Safe here because the daemon
    // never hosts a user's process: the shim, whose children would inherit this, does not
    // set it. Whatever a launch agent or an older version left behind is handled next.
    unsafe {
        libc::umask(0o077);
    }

    raise_fd_limit();
    let home = match paths::ensure_home() {
        Ok(home) => home,
        Err(e) => {
            eprintln!("logbeard: cannot create {}: {e}", paths::home().display());
            return 1;
        }
    };
    match paths::tighten_existing(&home) {
        Ok(0) => {}
        Ok(n) => log(&format!("restricted {n} existing file(s) to their owner")),
        // Worth a line, not worth refusing to capture.
        Err(e) => log(&format!("could not restrict existing files: {e}")),
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

    let daemon = match Daemon::open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("logbeard: cannot open storage: {e}");
            return 1;
        }
    };

    // Before the socket exists, not after: the socket becoming connectable is what announces
    // this daemon, and a stop arriving between the two would find the default disposition
    // still in place and kill the process outright — leaving behind the socket and the open
    // runs this handler exists to clean up.
    spawn_signal_handler(daemon.clone());

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

    spawn_spool_sweep(daemon.clone());
    spawn_maintenance(daemon.clone());
    spawn_retention(daemon.clone());
    watch_temporary_home(daemon.clone());

    // A blocking accept would wedge the process after a shutdown request, since nothing
    // else would connect to wake it.
    let _ = listener.set_nonblocking(true);
    accept_loop(&listener, &daemon);

    // Connection threads are still reading. Output a shim has handed to the socket counts as
    // delivered on its side, so it has to land before the runs close.
    std::thread::sleep(SHUTDOWN_DRAIN);
    let retired = daemon.retired.load(Ordering::SeqCst);
    daemon.close_active_runs();
    // Ours to remove: nothing else could have bound it while the lock was held.
    let _ = std::fs::remove_file(&socket);
    log("logbeard stopped");
    // Last, so the door stays shut until the socket is gone: a daemon starting the instant
    // this one lets go must not find a socket file with no listener behind it.
    lock.release();
    if retired {
        retire::EXIT_CODE
    } else {
        0
    }
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

/// Backfills spool files on a thread of its own: a large backlog must not hold up the
/// socket, and a run that never reached the daemon should appear without waiting for
/// another command to come along.
fn spawn_spool_sweep(daemon: Arc<Daemon>) {
    std::thread::Builder::new()
        .name("logbeard-spool".into())
        .spawn(move || {
            while !daemon.shutdown.load(Ordering::SeqCst) {
                let drained = spool::drain_all(&daemon);
                if drained > 0 {
                    log(&format!("backfilled {drained} spooled run(s)"));
                }
                std::thread::sleep(SPOOL_SWEEP);
            }
        })
        .ok();
}

/// Ticks live runs, so a quiet server's last entry shows up on time.
fn spawn_maintenance(daemon: Arc<Daemon>) {
    std::thread::spawn(move || {
        let mut watch = retire::Watch::start();
        while !daemon.shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(TICK);
            daemon.tick_runs();

            if watch.should_retire(daemon.active_run_count()) {
                log("this binary was replaced by an upgrade; exiting so the new one can start");
                daemon.retired.store(true, Ordering::SeqCst);
                daemon.shutdown.store(true, Ordering::SeqCst);
            }
        }
    });
}

/// Retention and tidying, on a thread of its own: a pass can take minutes on a large
/// project, and ticks have to keep coming through it.
fn spawn_retention(daemon: Arc<Daemon>) {
    std::thread::spawn(move || {
        let mut last_idle = std::time::Instant::now();
        while !daemon.shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(SWEEP_EVERY);
            daemon.retry_unfinished();
            for handle in daemon.open_projects() {
                let cap = daemon.retention_cap(&handle.project);
                match logbear_core::store::retention::enforce(&handle.db, cap) {
                    Ok(s) if s.squashed > 0 || s.evicted > 0 => log(&format!(
                        "{}: squashed {} and evicted the raw log of {} old run(s) to stay under {} bytes",
                        handle.project.name, s.squashed, s.evicted, cap
                    )),
                    Err(e) => log(&format!("{}: retention failed: {e}", handle.project.name)),
                    _ => {}
                }
            }
            if last_idle.elapsed() >= IDLE_EVERY {
                last_idle = std::time::Instant::now();
                daemon.close_idle_projects();
                daemon.release_idle_memory();
                let _ = daemon.index.checkpoint();
            }
        }
    });
}

/// Stops the daemon if a `LOGBEAR_HOME` it was given goes away.
///
/// Tests and `brew test` point the daemon at a temporary directory and then delete it, and
/// nothing else would ever stop the process. Only for an explicit `LOGBEAR_HOME`: the normal
/// home has no business exiting on its own. The check is by inode, so a new directory at the
/// same path, a later test's, does not keep this one alive.
fn watch_temporary_home(daemon: Arc<Daemon>) {
    use std::os::unix::fs::MetadataExt;

    if std::env::var_os("LOGBEAR_HOME").is_none() {
        return;
    }
    let inode = |path: &std::path::Path| std::fs::metadata(path).ok().map(|m| m.ino());
    let home = paths::home();
    let Some(ours) = inode(&home) else {
        return;
    };
    std::thread::spawn(move || {
        while !daemon.shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(HOME_CHECK_EVERY);
            if inode(&home) != Some(ours) {
                log(&format!("{} has gone; shutting down", home.display()));
                daemon.shutdown.store(true, Ordering::SeqCst);
            }
        }
    });
}

/// Asks for more file descriptors than a launch agent gets by default. Best effort: the
/// open-project limit is worked out from what this actually leaves.
fn raise_fd_limit() {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `limit` is a valid pointer for both calls.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return;
        }
        let wanted = (WANTED_FDS as libc::rlim_t).min(limit.rlim_max);
        if wanted > limit.rlim_cur {
            limit.rlim_cur = wanted;
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
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

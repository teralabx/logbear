//! `runlogd` — the long-lived process that owns the databases.
//!
//! It survives the desktop app being closed, because "keeps monitoring" is otherwise
//! false, and it auto-starts on the first `runlog` invocation so nobody has to think
//! about it. Everything it does happens behind one unix socket.

mod conn;
mod os_notify;
mod spool;
mod state;

use clap::{Parser, Subcommand};
use runlog_core::client::Client;
use runlog_core::paths;
use runlog_core::proto::Request;
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
#[command(name = "runlogd", version, about = "The runlog daemon")]
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
        eprintln!("runlogd: cannot create {}: {e}", paths::home().display());
        return 1;
    }

    // A stale socket file outlives an unclean shutdown. Probing it first is what
    // distinguishes "another daemon is running" from "the last one was killed".
    let socket = paths::socket();
    if socket.exists() {
        if Client::connect_to(&socket).is_ok() {
            eprintln!("runlogd: already running at {}", socket.display());
            return 0;
        }
        let _ = std::fs::remove_file(&socket);
    }

    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("runlogd: cannot bind {}: {e}", socket.display());
            return 1;
        }
    };
    // The socket is a private channel to this user's entire log history. 0600, always.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    }

    let daemon = match Daemon::open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("runlogd: cannot open storage: {e}");
            let _ = std::fs::remove_file(&socket);
            return 1;
        }
    };

    let _ = std::fs::write(paths::pid_file(), std::process::id().to_string());
    log(&format!(
        "runlogd {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        socket.display()
    ));

    let drained = spool::drain_all(&daemon);
    if drained > 0 {
        log(&format!("backfilled {drained} spooled run(s)"));
    }

    spawn_maintenance(daemon.clone());

    // A blocking accept would leave the process wedged after a shutdown request, since
    // nothing else would connect to wake it. Non-blocking plus a short sleep keeps the
    // loop responsive without a self-connect trick.
    let _ = listener.set_nonblocking(true);
    accept_loop(&listener, &daemon);

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(paths::pid_file());
    log("runlogd stopped");
    0
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
                    match runlog_core::store::retention::enforce(&handle.db, cap) {
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
            println!("runlogd: shutdown requested");
            0
        }
        Err(_) => {
            println!("runlogd: not running");
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
            eprintln!("runlogd: {e}");
            1
        }
    }
}

/// Timestamped stderr. Whatever starts the daemon redirects stdout and stderr to
/// `~/.runlog/runlogd.log`, so this is the log.
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

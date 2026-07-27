//! `runlog <cmd> [args...]` — wrap a command and capture what it emits.
//!
//! The ordering is load-bearing: read from the pty, write to the user's stdout, flush, and
//! only then hand a copy to [`crate::sink`], which cannot block. Whatever state the daemon
//! is in, it cannot add latency, change the exit code, or swallow a signal.
//!
//! The wrapped process is never modified. No environment variable is set and nothing is
//! injected; the child inherits exactly what the shell would have given it, and the only
//! observable difference is that its terminal is a pty runlog opened.

use crate::cli::WrapArgs;
use crate::sink::Sink;
use crate::term;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use runlog_core::client::Client;
use runlog_core::model::RunId;
use runlog_core::proto::{Request, Settings};
use std::ffi::CStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Exit code for "could not start it", matching the shell's convention.
const EXIT_CANNOT_RUN: i32 = 127;

/// Runs the wrapped command and exits with the child's exit code, verbatim.
pub fn run(args: WrapArgs) -> ! {
    term::install_panic_restore();
    match execute(args) {
        Ok(code) => {
            // `process::exit` skips destructors, so the raw-mode guard cannot be relied on.
            term::restore();
            std::process::exit(code)
        }
        Err(message) => {
            term::restore();
            eprintln!("runlog: {message}");
            std::process::exit(EXIT_CANNOT_RUN)
        }
    }
}

fn execute(args: WrapArgs) -> Result<i32, String> {
    if args.command.is_empty() {
        return Err("no command given".into());
    }
    // Resolved before anything is recorded. A command that cannot start produces no output
    // and no process, so it is not a run — and a history full of them buries the real ones.
    if resolve_program(&args.command[0]).is_none() {
        return Err(format!("cannot run {}: command not found", args.command[0]));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (cols, rows) = term::size();

    // Settings are fetched without starting the daemon, so a denylisted command does not
    // spin one up only to refuse to capture the session.
    let mut client = Client::connect().ok();
    let settings = client
        .as_mut()
        .and_then(|c| c.call::<Settings>(&Request::GetSettings).ok())
        .unwrap_or_default();

    if !args.force && is_denied(&args.command[0], &settings) {
        // A denylisted command is a session, not a run: no pty, no run record, and no
        // process left in between.
        exec_direct(&args.command);
    }

    if client.is_none() {
        client = Client::connect_or_start().ok();
    }

    let token = crate::sink::local_token();
    if client.is_some() {
        // Drain the backlog on its own connection and thread, so it never delays the
        // command the user just typed.
        let skip = token.clone();
        std::thread::Builder::new()
            .name("runlog-drain".into())
            .spawn(move || crate::sink::drain(&skip))
            .ok();
    }

    let stamp = runlog_core::project::stamp(args.command.clone(), &cwd, cols, rows);
    let started_at = runlog_core::now_micros();

    let run_id = client.as_mut().and_then(|c| start_run(c, &stamp));
    if run_id.is_none() {
        // Registered nowhere: spool from the first byte rather than lose the run.
        client = None;
    }

    let sink = Sink::start(client, run_id, token, stamp, started_at);
    let outcome = if args.no_pty {
        run_piped(&args.command, &cwd, &sink)
    } else {
        run_pty(&args.command, &cwd, cols, rows, &sink)
    };

    match outcome {
        Ok(o) => {
            sink.finish(Some(o.code as i32), o.signal.map(signal_name));
            Ok(o.exit_code())
        }
        Err(e) => {
            sink.finish(Some(EXIT_CANNOT_RUN), None);
            Err(e)
        }
    }
}

fn start_run(client: &mut Client, stamp: &runlog_core::model::RunStamp) -> Option<RunId> {
    let value = client
        .call_value(&Request::StartRun {
            stamp: stamp.clone(),
        })
        .ok()?;
    // Accepts a bare id or `{"run_id": n}`, so a daemon that changes its response shape
    // does not silently turn every run into a spool file.
    value
        .as_i64()
        .or_else(|| value.get("run_id").and_then(|v| v.as_i64()))
        .or_else(|| value.get("id").and_then(|v| v.as_i64()))
}

/// Resolves `program` the way a shell would, returning the path that would be executed.
///
/// A name containing `/` is taken as a path; anything else is looked up on `PATH`. `None`
/// means there is nothing to run.
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    let is_executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        })
    };

    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable(&path).then_some(path);
    }
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| is_executable(candidate))
}

/// Whether `program` is denylisted, matched on the basename so `/usr/bin/vim` and `vim`
/// are the same decision.
pub fn is_denied(program: &str, settings: &Settings) -> bool {
    let base = Path::new(program)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    settings.denylist.iter().any(|d| d == &base)
}

/// Replaces this process with the real command.
///
/// An exec rather than a spawn, so signals, job control, the terminal and the exit code are
/// exactly what they would have been without runlog.
fn exec_direct(command: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&command[0])
        .args(&command[1..])
        .exec();
    eprintln!("runlog: cannot run {}: {err}", command[0]);
    std::process::exit(EXIT_CANNOT_RUN)
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

pub struct Outcome {
    pub code: u32,
    pub signal: Option<i32>,
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        exit_code_for(self.code, self.signal)
    }
}

/// The exit code a shell would report: `128 + signum` for a signalled child.
///
/// Scripts test `$? -eq 130` for Ctrl-C, so anything else changes the meaning of the
/// wrapped command.
pub fn exit_code_for(code: u32, signal: Option<i32>) -> i32 {
    match signal {
        Some(s) => 128 + s,
        None => (code & 0xff) as i32,
    }
}

pub fn signal_name(n: i32) -> String {
    let name = match n {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGABRT => "SIGABRT",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGBUS => "SIGBUS",
        libc::SIGSYS => "SIGSYS",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        _ => return format!("SIG{n}"),
    };
    name.to_string()
}

/// Recovers a signal number from libc's description of it.
///
/// `portable-pty` reports a signalled exit as the description ("Interrupt") rather than a
/// number. Matching through `strsignal` rather than a hand-written table keeps the two
/// sides consistent whatever the platform and locale make those strings.
pub fn signal_number(description: &str) -> Option<i32> {
    if let Some(rest) = description.strip_prefix("Signal ") {
        return rest.trim().parse().ok();
    }
    (1..=31).find(|n| strsignal(*n).as_deref() == Some(description))
}

fn strsignal(n: i32) -> Option<String> {
    let ptr = unsafe { libc::strsignal(n) };
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// The pty path
// ---------------------------------------------------------------------------

fn run_pty(
    command: &[String],
    cwd: &Path,
    cols: u16,
    rows: u16,
    sink: &Sink,
) -> Result<Outcome, String> {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("could not open a pty: {e}"))?;

    let mut builder = CommandBuilder::new(&command[0]);
    builder.args(&command[1..]);
    builder.cwd(cwd);

    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| format!("cannot run {}: {e}", command[0]))?;

    // The parent must drop the slave or the master never sees EOF: the kernel keeps the
    // pty open while any process holds a slave fd, and the read loop would hang forever
    // after the child exits.
    drop(pair.slave);

    let master = pair.master;
    let master_fd = master.as_raw_fd();
    let child_pid = child.process_id().map(|p| p as i32);
    let mut reader = master
        .try_clone_reader()
        .map_err(|e| format!("could not read from the pty: {e}"))?;
    let writer = master
        .take_writer()
        .map_err(|e| format!("could not write to the pty: {e}"))?;

    // Leaves the pty's line discipline as the only one in play. Restored on every exit
    // path, including panics.
    let _raw = term::raw_mode(libc::STDIN_FILENO);

    spawn_stdin_forwarder(writer, master_fd);
    spawn_signal_forwarder(master_fd, child_pid, sink.handle());

    pump(&mut reader, sink);

    let status = child
        .wait()
        .map_err(|e| format!("lost track of the child process: {e}"))?;
    Ok(Outcome {
        code: status.exit_code(),
        signal: status.signal().and_then(signal_number),
    })
}

/// Read → user's stdout → flush → daemon, in that order.
fn pump(reader: &mut (impl Read + ?Sized), sink: &Sink) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                if out.write_all(chunk).is_err() {
                    break;
                }
                let _ = out.flush();
                // Timestamped here rather than on the sender thread so arrival times stay
                // real under load.
                sink.offer(runlog_core::now_micros(), chunk);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // A pty master reports the child's exit as EIO on Linux rather than as a clean
            // EOF, so any error means the stream is over.
            Err(_) => break,
        }
    }
}

fn spawn_stdin_forwarder(mut writer: Box<dyn Write + Send>, master_fd: Option<i32>) {
    // Never joined: a blocking read on stdin cannot be cancelled portably, and the process
    // exits and takes the thread with it.
    std::thread::Builder::new()
        .name("runlog-stdin".into())
        .spawn(move || {
            let interactive = crate::ui::is_tty(libc::STDIN_FILENO);
            let mut forwarded_anything = false;
            let mut buf = [0u8; 8192];
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            loop {
                match input.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if writer.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                        forwarded_anything = true;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            if interactive || forwarded_anything {
                send_eof(&mut writer, master_fd);
            }
            // Dropping `writer` makes portable-pty send a newline as well as an EOT. That
            // newline is a byte the child never received from the user: it echoes back and
            // lands in the captured stream as a phantom blank line. Leaking one fd for the
            // remaining life of the process is the cheaper mistake.
            std::mem::forget(writer);
        })
        .ok();
}

/// Sends the pty's EOF character now that our own stdin has ended, or `echo x | runlog cat`
/// never returns.
///
/// A pty can only signal EOF through the line discipline's EOF character, the same byte
/// Ctrl-D sends. Only sent when there was a real stdin behind it — an interactive terminal,
/// or a pipe that delivered something — because under `runlog npm test < /dev/null` the pty
/// echoes it as `^D` into the first bytes of the captured stream.
fn send_eof(writer: &mut Box<dyn Write + Send>, master_fd: Option<i32>) {
    let Some(fd) = master_fd else { return };
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return;
    }
    let eof = termios.c_cc[libc::VEOF];
    if eof != 0 {
        let _ = writer.write_all(&[eof]);
        let _ = writer.flush();
    }
}

fn spawn_signal_forwarder(master_fd: Option<i32>, child_pid: Option<i32>, sink: crate::sink::SinkHandle) {
    use signal_hook::consts::signal::*;
    use signal_hook::iterator::Signals;

    let Ok(mut signals) = Signals::new([SIGWINCH, SIGINT, SIGTERM, SIGQUIT, SIGHUP]) else {
        return;
    };
    std::thread::Builder::new()
        .name("runlog-signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                if sig == SIGWINCH {
                    let (cols, rows) = term::size();
                    if let Some(fd) = master_fd {
                        term::set_size_of_fd(fd, cols, rows);
                    }
                    sink.resize(cols, rows);
                    continue;
                }
                // The process group, not the pid: signalling only a supervisor's leader
                // leaves its children running and makes Ctrl-C look broken. `tcgetpgrp`
                // on the master gives the pty's current foreground group, which is right
                // even after the child hands control to something it spawned.
                if let Some(pgrp) = foreground_group(master_fd).or(child_pid) {
                    unsafe { libc::killpg(pgrp, sig) };
                }
            }
        })
        .ok();
}

fn foreground_group(master_fd: Option<i32>) -> Option<i32> {
    let fd = master_fd?;
    match unsafe { libc::tcgetpgrp(fd) } {
        pid if pid > 0 => Some(pid),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The pipe path (`--no-pty`)
// ---------------------------------------------------------------------------

/// The `--no-pty` escape hatch, for the rare program a pty upsets.
///
/// Timing fidelity is degraded and cannot be fixed here: libc fully buffers stdout at 4 KB
/// when `isatty()` is false, so every line in a clump arrives at the same instant and
/// per-line timestamps become fiction. Interactive tooling stops working for the same
/// reason.
fn run_piped(command: &[String], cwd: &Path, sink: &Sink) -> Result<Outcome, String> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::{Command, Stdio};

    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("could not create a pipe: {}", std::io::Error::last_os_error()));
    }
    let (read_end, write_end) = unsafe {
        (
            OwnedFd::from_raw_fd(fds[0]),
            OwnedFd::from_raw_fd(fds[1]),
        )
    };
    let write_dup = write_end
        .try_clone()
        .map_err(|e| format!("could not duplicate the pipe: {e}"))?;

    // Scoped so the parent's copies of the write end close before the read loop starts;
    // otherwise the pipe never reaches EOF and the loop hangs after the child has exited.
    // Both fds point at one pipe, so stdout and stderr stay interleaved in the order the
    // child wrote them, matching the pty path's single-stream shape.
    let mut child = {
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..])
            .current_dir(cwd)
            .stdout(Stdio::from(write_end))
            .stderr(Stdio::from(write_dup));
        cmd.spawn()
            .map_err(|e| format!("cannot run {}: {e}", command[0]))?
    };

    let child_pid = child.id() as i32;
    spawn_pipe_signal_forwarder(child_pid);

    let mut reader = std::fs::File::from(read_end);
    pump(&mut reader, sink);

    let status = child
        .wait()
        .map_err(|e| format!("lost track of the child process: {e}"))?;
    use std::os::unix::process::ExitStatusExt;
    Ok(Outcome {
        code: status.code().unwrap_or(1) as u32,
        signal: status.signal(),
    })
}

/// Forwards signals to the child pid rather than the process group.
///
/// Pipe mode leaves the child in our own group, where the terminal already delivers Ctrl-C
/// to it directly and `killpg` would signal the shim as well and loop. This covers only the
/// case where something signals the shim from outside.
fn spawn_pipe_signal_forwarder(child_pid: i32) {
    use signal_hook::consts::signal::*;
    use signal_hook::iterator::Signals;

    let Ok(mut signals) = Signals::new([SIGINT, SIGTERM, SIGQUIT, SIGHUP]) else {
        return;
    };
    std::thread::Builder::new()
        .name("runlog-signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                unsafe { libc::kill(child_pid, sig) };
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_on_path_resolves() {
        assert!(resolve_program("sh").is_some());
        assert!(resolve_program("/bin/sh").is_some());
    }

    #[test]
    fn a_program_that_does_not_exist_resolves_to_nothing() {
        // What `runlog $c` produces in fish when `$c` is the string "logs -n 50": one
        // argument, no such executable, and nothing worth recording as a run.
        assert!(resolve_program("logs -n 50").is_none());
        assert!(resolve_program("definitely-not-a-real-program").is_none());
        assert!(resolve_program("/bin/definitely-not-a-real-program").is_none());
    }

    #[test]
    fn a_directory_on_path_is_not_a_program() {
        assert!(resolve_program("/tmp").is_none());
    }

    #[test]
    fn a_file_without_the_execute_bit_is_not_a_program() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-executable");
        std::fs::write(&file, b"#!/bin/sh\n").unwrap();
        assert!(resolve_program(&file.to_string_lossy()).is_none());
    }

    #[test]
    fn a_clean_exit_is_passed_through_verbatim() {
        assert_eq!(exit_code_for(0, None), 0);
        assert_eq!(exit_code_for(1, None), 1);
        assert_eq!(exit_code_for(42, None), 42);
        assert_eq!(exit_code_for(255, None), 255);
    }

    #[test]
    fn a_signalled_child_exits_the_way_a_shell_reports_it() {
        assert_eq!(exit_code_for(1, Some(libc::SIGINT)), 130);
        assert_eq!(exit_code_for(1, Some(libc::SIGTERM)), 143);
        assert_eq!(exit_code_for(1, Some(libc::SIGKILL)), 137);
        assert_eq!(exit_code_for(1, Some(libc::SIGQUIT)), 131);
        assert_eq!(exit_code_for(1, Some(libc::SIGHUP)), 129);
    }

    #[test]
    fn the_signal_beats_the_exit_code_when_both_are_present() {
        // portable-pty fills in a code of 1 alongside the signal; the signal is the truth.
        assert_eq!(exit_code_for(1, Some(libc::SIGINT)), 130);
        assert_ne!(exit_code_for(1, Some(libc::SIGINT)), 1);
    }

    #[test]
    fn signal_numbers_round_trip_through_libcs_own_descriptions() {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            let description = strsignal(sig).expect("libc knows this signal");
            assert_eq!(
                signal_number(&description),
                Some(sig),
                "could not recover {sig} from {description:?}"
            );
        }
    }

    #[test]
    fn an_unknown_description_falls_back_to_the_numeric_form() {
        assert_eq!(signal_number("Signal 61"), Some(61));
        assert_eq!(signal_number("something libc never said"), None);
    }

    #[test]
    fn signal_names_are_canonical() {
        assert_eq!(signal_name(libc::SIGINT), "SIGINT");
        assert_eq!(signal_name(libc::SIGTERM), "SIGTERM");
        assert_eq!(signal_name(99), "SIG99");
    }

    #[test]
    fn the_denylist_matches_on_the_basename() {
        let settings = Settings::default();
        assert!(is_denied("psql", &settings));
        assert!(is_denied("/usr/local/bin/psql", &settings));
        assert!(is_denied("vim", &settings));
        assert!(!is_denied("mix", &settings));
        assert!(!is_denied("psqlx", &settings));
        // A directory called `vim` on the way to something else must not match.
        assert!(!is_denied("/opt/vim/bin/mix", &settings));
    }

}

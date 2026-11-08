//! `logbear <cmd> [args...]` — wrap a command and capture what it emits.
//!
//! The child starts first, before anything else is attempted, and nothing it does can wait
//! on the daemon: contact is made in the background by [`crate::sink`]. Output is read, then
//! written to the user's stream, and only then copied to the sink, which cannot block.
//!
//! Each standard stream is treated on its own, so the child sees exactly what it would have
//! seen without logbear:
//!
//! - A stream on a terminal gets a pty, sized to match, so colours, progress bars and line
//!   buffering all behave.
//! - Any other stdin is inherited untouched, so EOF, binary bytes and long lines all reach
//!   the child with no line discipline in the way.
//! - Any other stdout or stderr gets a pipe that logbear copies to the real destination, so
//!   `isatty` is false and the bytes are exact. When the reader downstream goes away, logbear
//!   closes the pipe, and the child gets its SIGPIPE just as it would have.
//!
//! The wrapped process is never modified. No environment variable is set and nothing is
//! injected.

use crate::cli::WrapArgs;
use crate::sink::{RunInfo, Sink, SinkHandle};
use crate::term;
use logbear_core::denylist::is_denied_command;
use logbear_core::proto::Settings;
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Exit code for "could not start it", matching the shell's convention.
const EXIT_CANNOT_RUN: i32 = 127;

/// How long output keeps being read once the child has exited. A background process it left
/// holding the stream open must not hold the command open too.
const AFTER_EXIT: Duration = Duration::from_millis(250);

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
            eprintln!("logbear: {message}");
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

    // Read from disk rather than asked of the daemon, which may be down or wedged.
    if !args.force && is_denied_command(&args.command, &Settings::load()) {
        // A denylisted command is a session, not a run: no pty, no run record, and no
        // process left in between.
        exec_direct(&args.command);
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (cols, rows) = term::size();
    let plan = Plan::detect(args.no_pty);
    let started_at = logbear_core::now_micros();
    let mut spawned = spawn(&args.command, &cwd, cols, rows, &plan)?;

    let sink = Sink::start(RunInfo {
        command: args.command.clone(),
        cwd,
        cols,
        rows,
        started_at,
        token: crate::sink::local_token(),
    });

    let status = supervise(&mut spawned, &sink, &plan)?;
    let signal = status.signal();
    let code = status.code().unwrap_or(1) as u32;
    sink.finish(Some(code as i32), signal.map(signal_name));
    Ok(exit_code_for(code, signal))
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

/// Replaces this process with the real command.
///
/// An exec rather than a spawn, so signals, job control, the terminal and the exit code are
/// exactly what they would have been without logbear.
fn exec_direct(command: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&command[0])
        .args(&command[1..])
        .exec();
    eprintln!("logbear: cannot run {}: {err}", command[0]);
    std::process::exit(EXIT_CANNOT_RUN)
}

// ---------------------------------------------------------------------------
// Exit status
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Which stream gets what
// ---------------------------------------------------------------------------

/// What each standard stream of the child is attached to.
struct Plan {
    stdin_on_pty: bool,
    stdout_on_pty: bool,
    stderr_on_pty: bool,
    /// stdout and stderr are the same destination, so they share one pipe and stay in the
    /// order the child wrote them.
    merged: bool,
}

impl Plan {
    fn detect(no_pty: bool) -> Plan {
        Plan::decide(
            no_pty,
            crate::ui::is_tty(libc::STDIN_FILENO) && term::in_foreground(libc::STDIN_FILENO),
            crate::ui::is_tty(libc::STDOUT_FILENO),
            crate::ui::is_tty(libc::STDERR_FILENO),
            same_destination(libc::STDOUT_FILENO, libc::STDERR_FILENO),
        )
    }

    fn decide(no_pty: bool, stdin: bool, stdout: bool, stderr: bool, same: bool) -> Plan {
        Plan {
            stdin_on_pty: !no_pty && stdin,
            stdout_on_pty: !no_pty && stdout,
            stderr_on_pty: !no_pty && stderr,
            merged: same && (no_pty || (!stdout && !stderr)),
        }
    }

    fn needs_pty(&self) -> bool {
        self.stdin_on_pty || self.stdout_on_pty || self.stderr_on_pty
    }
}

fn same_destination(a: RawFd, b: RawFd) -> bool {
    let stat = |fd| {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        (unsafe { libc::fstat(fd, &mut st) } == 0).then_some((st.st_dev, st.st_ino))
    };
    matches!((stat(a), stat(b)), (Some(x), Some(y)) if x == y)
}

fn close_on_exec(fd: RawFd) {
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
}

fn open_pty(cols: u16, rows: u16) -> Result<(OwnedFd, OwnedFd), String> {
    let (mut master, mut slave) = (0, 0);
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // The child's terminal starts from the user's own settings where there are any, so
    // erase characters, flow control and the like match.
    let mut settings: libc::termios = unsafe { std::mem::zeroed() };
    let inherited = [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO]
        .iter()
        .any(|fd| unsafe { libc::tcgetattr(*fd, &mut settings) } == 0);
    let settings_ptr: *mut libc::termios = if inherited {
        &mut settings
    } else {
        std::ptr::null_mut()
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            settings_ptr,
            &raw mut size,
        )
    };
    if rc != 0 {
        return Err(format!("could not open a pty: {}", std::io::Error::last_os_error()));
    }
    close_on_exec(master);
    close_on_exec(slave);
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

fn open_pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("could not create a pipe: {}", std::io::Error::last_os_error()));
    }
    close_on_exec(fds[0]);
    close_on_exec(fds[1]);
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

// ---------------------------------------------------------------------------
// Spawning
// ---------------------------------------------------------------------------

/// A stream of output to copy, and where to.
struct Source {
    read: OwnedFd,
    dest: RawFd,
    capture: bool,
    is_pty: bool,
}

struct Spawned {
    child: Child,
    master: Option<OwnedFd>,
    sources: Vec<Source>,
}

fn stdio_of(fd: &OwnedFd) -> Result<Stdio, String> {
    fd.try_clone()
        .map(Stdio::from)
        .map_err(|e| format!("could not duplicate a descriptor: {e}"))
}

fn spawn(
    command: &[String],
    cwd: &Path,
    cols: u16,
    rows: u16,
    plan: &Plan,
) -> Result<Spawned, String> {
    let pty = plan.needs_pty().then(|| open_pty(cols, rows)).transpose()?;

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]).current_dir(cwd);

    let mut sources = Vec::new();
    if let Some((_, slave)) = &pty {
        if plan.stdin_on_pty {
            cmd.stdin(stdio_of(slave)?);
        }
        if plan.stdout_on_pty {
            cmd.stdout(stdio_of(slave)?);
        }
        if plan.stderr_on_pty {
            cmd.stderr(stdio_of(slave)?);
        }
    }
    if !plan.stdout_on_pty {
        let (read, write) = open_pipe()?;
        if plan.merged {
            cmd.stderr(stdio_of(&write)?);
        }
        cmd.stdout(Stdio::from(write));
        sources.push(Source {
            read,
            dest: libc::STDOUT_FILENO,
            capture: true,
            is_pty: false,
        });
    }
    if !plan.stderr_on_pty && !plan.merged {
        let (read, write) = open_pipe()?;
        cmd.stderr(Stdio::from(write));
        sources.push(Source {
            read,
            dest: libc::STDERR_FILENO,
            capture: true,
            is_pty: false,
        });
    }
    if let Some((master, slave)) = &pty {
        // A session of its own, with the pty as its controlling terminal: that is what makes
        // Ctrl-C, window-size changes and job control work inside it.
        let slave_fd = slave.as_raw_fd();
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let read = master
            .try_clone()
            .map_err(|e| format!("could not read from the pty: {e}"))?;
        let dest = if plan.stdout_on_pty {
            libc::STDOUT_FILENO
        } else if plan.stderr_on_pty {
            libc::STDERR_FILENO
        } else {
            // Only stdin is on the pty, so all that can come back is the echo of what the
            // user is typing. It belongs on their terminal and is not the program's output.
            libc::STDIN_FILENO
        };
        sources.push(Source {
            read,
            dest,
            capture: plan.stdout_on_pty || plan.stderr_on_pty,
            is_pty: true,
        });
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", command[0]))?;

    // Dropping `cmd` closes the parent's copies of the write ends and of the slave. Any left
    // open would keep the read loops waiting after the child has gone.
    drop(cmd);
    let master = pty.map(|(master, _slave)| master);
    Ok(Spawned { child, master, sources })
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

fn supervise(
    spawned: &mut Spawned,
    sink: &Sink,
    plan: &Plan,
) -> Result<std::process::ExitStatus, String> {
    let exited = AtomicBool::new(false);
    let master_fd = spawned.master.as_ref().map(|m| m.as_raw_fd());
    let child_pid = spawned.child.id() as i32;
    let sources = std::mem::take(&mut spawned.sources);

    // Leaves the pty's line discipline as the only one in play. Restored on every exit path,
    // including panics.
    let _raw = if plan.stdin_on_pty {
        term::raw_mode(libc::STDIN_FILENO)
    } else {
        None
    };

    match master_fd {
        Some(master) => {
            // The terminal must stay usable by a job moved to the background: with these
            // ignored, touching it fails with an error rather than stopping logbear.
            unsafe {
                libc::signal(libc::SIGTTIN, libc::SIG_IGN);
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            }
            if plan.stdin_on_pty {
                spawn_stdin_forwarder(master, child_pid);
            }
            spawn_signal_forwarder(master, child_pid, sink.handle());
        }
        None => spawn_pipe_signal_forwarder(child_pid),
    }

    std::thread::scope(|scope| {
        for source in sources {
            let exited = &exited;
            scope.spawn(move || pump(source, sink, exited));
        }
        let status = spawned
            .child
            .wait()
            .map_err(|e| format!("lost track of the child process: {e}"));
        exited.store(true, Ordering::SeqCst);
        status
    })
}

/// Waits for output, giving up shortly after the child has exited.
fn wait_readable(fd: RawFd, exited: &AtomicBool, exited_at: &mut Option<Instant>) -> bool {
    loop {
        if exited.load(Ordering::SeqCst) && exited_at.is_none() {
            *exited_at = Some(Instant::now());
        }
        if exited_at.is_some_and(|at| at.elapsed() > AFTER_EXIT) {
            return false;
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 20) };
        if ready > 0 {
            return true;
        }
        if ready < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
        // Nothing buffered and the child has gone: whatever still holds this open is a
        // background process, not the command.
        if ready == 0 && exited_at.is_some() {
            return false;
        }
    }
}

/// Read → user's stream → daemon, in that order.
fn pump(source: Source, sink: &Sink, exited: &AtomicBool) {
    let fd = source.read.as_raw_fd();
    let mut reader = std::fs::File::from(source.read);
    let mut buf = vec![0u8; 64 * 1024];
    let mut exited_at = None;
    let mut delivering = true;
    while wait_readable(fd, exited, &mut exited_at) {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                if delivering && write_all(source.dest, chunk).is_err() {
                    if !source.is_pty {
                        // The reader has gone. Returning closes our end, so the child gets
                        // the SIGPIPE it would have had without logbear.
                        return;
                    }
                    // The terminal has gone. A pty's other side has no SIGPIPE, so keep
                    // draining or the child blocks forever on a full buffer.
                    delivering = false;
                }
                if source.capture {
                    // Timestamped here rather than on the sender thread so arrival times
                    // stay real under load.
                    sink.offer(logbear_core::now_micros(), chunk);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // A pty master reports the child's exit as EIO on Linux rather than as a clean
            // EOF, so any error means the stream is over.
            Err(_) => break,
        }
    }
}

/// Writes to one of our own standard streams, which we do not own and must not close.
///
/// Waits rather than failing on a non-blocking descriptor: node and others set `O_NONBLOCK`
/// on a pipe they share with us.
fn write_all(fd: RawFd, bytes: &[u8]) -> std::io::Result<()> {
    let mut out = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
    let mut rest = bytes;
    while !rest.is_empty() {
        match out.write(rest) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let mut poll = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                unsafe { libc::poll(&mut poll, 1, 100) };
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Copies keystrokes to the pty. Only runs when stdin is a terminal; anything else is
/// inherited by the child directly.
fn spawn_stdin_forwarder(master: RawFd, child_pid: i32) {
    // Never joined: a blocking read on stdin cannot be cancelled portably, and the process
    // exits and takes the thread with it.
    std::thread::Builder::new()
        .name("logbear-stdin".into())
        .spawn(move || {
            let mut stdin = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(libc::STDIN_FILENO) });
            let mut buf = [0u8; 8192];
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // Reading from the terminal while in the background fails. That lasts
                    // until the job comes back with `fg`.
                    Err(_) if !term::in_foreground(libc::STDIN_FILENO) => {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    Err(_) => return,
                };
                let mut rest = &buf[..n];
                while !rest.is_empty() {
                    let (before, after) = split_at_suspend(rest, suspend_byte(master));
                    if write_all(master, before).is_err() {
                        return;
                    }
                    rest = match after {
                        Some(after) => {
                            suspend(master, child_pid);
                            after
                        }
                        None => &[],
                    };
                }
            }
        })
        .ok();
}

/// The byte the program inside the pty has asked to mean "suspend", if it wants one at all.
/// A full-screen program that switched this off wants Ctrl-Z for itself.
fn suspend_byte(master: RawFd) -> Option<u8> {
    let mut settings: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(master, &mut settings) } != 0 {
        return None;
    }
    if settings.c_lflag & libc::ISIG == 0 {
        return None;
    }
    match settings.c_cc[libc::VSUSP] {
        0 => None,
        byte => Some(byte),
    }
}

/// Splits `bytes` around the first suspend byte, which is consumed rather than forwarded.
fn split_at_suspend(bytes: &[u8], suspend: Option<u8>) -> (&[u8], Option<&[u8]>) {
    match suspend.and_then(|s| bytes.iter().position(|b| *b == s)) {
        Some(i) => (&bytes[..i], Some(&bytes[i + 1..])),
        None => (bytes, None),
    }
}

/// Does what Ctrl-Z would have done had logbear not been in between.
///
/// The child leads its own session, so the kernel treats its process group as orphaned and
/// discards SIGTSTP. SIGSTOP is not subject to that. Once it has stopped, logbear stops
/// itself so the shell sees a suspended job, and passes the SIGCONT on when it is resumed.
fn suspend(master: RawFd, child_pid: i32) {
    let group = foreground_group(master).unwrap_or(child_pid);
    unsafe { libc::killpg(group, libc::SIGSTOP) };
    term::restore();
    unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) };

    // Running again: back in the foreground with `fg`, or left in the background with `bg`.
    if term::in_foreground(libc::STDIN_FILENO) {
        std::mem::forget(term::raw_mode(libc::STDIN_FILENO));
    }
    let (cols, rows) = term::size();
    term::set_size_of_fd(master, cols, rows);
    unsafe { libc::killpg(group, libc::SIGCONT) };
}

fn spawn_signal_forwarder(master: RawFd, child_pid: i32, sink: SinkHandle) {
    use signal_hook::consts::signal::*;
    use signal_hook::iterator::Signals;

    let Ok(mut signals) = Signals::new([SIGWINCH, SIGINT, SIGTERM, SIGQUIT, SIGHUP, SIGTSTP]) else {
        return;
    };
    std::thread::Builder::new()
        .name("logbear-signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                match sig {
                    SIGWINCH => {
                        let (cols, rows) = term::size();
                        term::set_size_of_fd(master, cols, rows);
                        sink.resize(cols, rows);
                    }
                    SIGTSTP => suspend(master, child_pid),
                    // The process group, not the pid: signalling only a supervisor's leader
                    // leaves its children running and makes Ctrl-C look broken. `tcgetpgrp`
                    // on the master gives the pty's current foreground group, which is right
                    // even after the child hands control to something it spawned.
                    _ => {
                        let group = foreground_group(master).unwrap_or(child_pid);
                        unsafe { libc::killpg(group, sig) };
                    }
                }
            }
        })
        .ok();
}

fn foreground_group(master: RawFd) -> Option<i32> {
    match unsafe { libc::tcgetpgrp(master) } {
        pid if pid > 0 => Some(pid),
        _ => None,
    }
}

/// With no pty the child shares our process group, where the terminal already delivers
/// Ctrl-C to it directly. Forwarding that as well would send it twice, and plenty of servers
/// treat a second Ctrl-C as "stop waiting and die". This covers signals sent to logbear
/// alone from outside.
fn spawn_pipe_signal_forwarder(child_pid: i32) {
    use signal_hook::consts::signal::*;
    use signal_hook::iterator::Signals;

    let Ok(mut signals) = Signals::new([SIGINT, SIGTERM, SIGQUIT, SIGHUP]) else {
        return;
    };
    std::thread::Builder::new()
        .name("logbear-signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                if matches!(sig, SIGINT | SIGQUIT) && terminal_delivers_to_us() {
                    continue;
                }
                unsafe { libc::kill(child_pid, sig) };
            }
        })
        .ok();
}

/// Whether we are the foreground job of a controlling terminal.
fn terminal_delivers_to_us() -> bool {
    let Ok(tty) = std::fs::File::open("/dev/tty") else {
        return false;
    };
    term::in_foreground(tty.as_raw_fd())
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
        // What `logbear $c` produces in fish when `$c` is the string "logs -n 50": one
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
    fn each_stream_is_decided_on_its_own() {
        let plan = Plan::decide(false, false, true, true, true);
        assert!(!plan.stdin_on_pty && plan.stdout_on_pty && plan.stderr_on_pty);
        assert!(!plan.merged, "a terminal needs no shared pipe");

        let plan = Plan::decide(false, true, false, true, false);
        assert!(plan.stdin_on_pty && !plan.stdout_on_pty && plan.stderr_on_pty);

        let plan = Plan::decide(false, false, false, false, true);
        assert!(!plan.needs_pty());
        assert!(plan.merged, "2>&1 has to keep its order");

        let plan = Plan::decide(false, false, false, false, false);
        assert!(!plan.merged, "stderr to one place, stdout to another");
    }

    #[test]
    fn no_pty_means_no_pty_even_on_a_terminal() {
        let plan = Plan::decide(true, true, true, true, true);
        assert!(!plan.needs_pty());
        assert!(plan.merged);
    }

    #[test]
    fn the_suspend_byte_is_consumed_and_the_rest_survives() {
        let (before, after) = split_at_suspend(b"ab\x1acd", Some(0x1a));
        assert_eq!(before, b"ab");
        assert_eq!(after, Some(&b"cd"[..]));
    }

    #[test]
    fn a_program_that_turned_off_suspend_gets_the_byte() {
        let (before, after) = split_at_suspend(b"ab\x1acd", None);
        assert_eq!(before, b"ab\x1acd");
        assert_eq!(after, None);
    }

    #[test]
    fn signal_names_are_canonical() {
        assert_eq!(signal_name(libc::SIGINT), "SIGINT");
        assert_eq!(signal_name(libc::SIGTERM), "SIGTERM");
        assert_eq!(signal_name(99), "SIG99");
    }

}

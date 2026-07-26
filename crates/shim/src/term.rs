//! The user's terminal: its size, and its line discipline.
//!
//! Two things here decide whether runlog is tolerable to have installed.
//!
//! **Size.** The pty handed to the child is sized to the user's *real* terminal, not to a
//! guess. A program that reads `COLUMNS` and hard-wraps at 80 while the window is 200 wide
//! looks broken, and the user blames the wrapper, correctly.
//!
//! **Raw mode.** Forwarding keystrokes to the child means our own tty must stop
//! interpreting them — no echo, no line buffering, no local signal generation — so the
//! pty's line discipline is the only one in play and `IEx.pry`, `rails console` and Ctrl-C
//! behave exactly as they would unwrapped. The cost is that a process which exits without
//! restoring the terminal leaves the user with an invisible, unresponsive shell. So the
//! original termios is stashed in a global the moment it is taken, and *every* exit path —
//! normal return, guard drop, panic hook, `process::exit` — goes through [`restore`].

use std::os::fd::RawFd;
use std::sync::Mutex;

/// What a terminal is when nothing will tell us otherwise. The same fallback `stty` uses.
pub const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// A zero in a `winsize` is the kernel saying "I don't know", not a zero-width terminal —
/// it is what a pipe, a cron job, or a CI runner reports. Treated as absent so the caller
/// falls through to the next fd and finally to the default.
pub fn size_from_winsize(cols: u16, rows: u16) -> Option<(u16, u16)> {
    if cols == 0 {
        return None;
    }
    Some((cols, if rows == 0 { DEFAULT_SIZE.1 } else { rows }))
}

/// First fd that knows the answer wins. stdin is asked first because it is the one still
/// attached to the terminal when stdout has been redirected to a file.
pub fn size() -> (u16, u16) {
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if let Some(sz) = size_of_fd(fd) {
            return sz;
        }
    }
    DEFAULT_SIZE
}

pub fn size_of_fd(fd: RawFd) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) };
    if rc != 0 {
        return None;
    }
    size_from_winsize(ws.ws_col, ws.ws_row)
}

/// Push a size onto the pty master. Done by raw ioctl rather than through `MasterPty`
/// because the signal-watching thread needs it and the master handle is not `Sync`.
pub fn set_size_of_fd(fd: RawFd, cols: u16, rows: u16) -> bool {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) == 0 }
}

// ---------------------------------------------------------------------------
// Raw mode
// ---------------------------------------------------------------------------

/// Set once, read from anywhere, because the restore has to be reachable from the panic
/// hook and from the exit path as well as from the guard's `Drop`.
static ORIGINAL: Mutex<Option<(RawFd, libc::termios)>> = Mutex::new(None);

/// Restores the terminal when dropped. Held for the lifetime of the wrapped run; the
/// explicit [`restore`] call on the exit path exists because `process::exit` skips drops.
pub struct RawGuard {
    _private: (),
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Put `fd` into raw mode. Returns `None` when the fd is not a terminal, which is the
/// normal case under CI or a pipe and is not an error — there is simply no line discipline
/// of ours to get out of the way.
pub fn raw_mode(fd: RawFd) -> Option<RawGuard> {
    if unsafe { libc::isatty(fd) } != 1 {
        return None;
    }
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
        return None;
    }
    let mut raw = original;
    unsafe { libc::cfmakeraw(&mut raw) };
    // Return from read() as soon as a single byte is available: a keystroke has to reach
    // the child now, not once a buffer fills.
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    if let Ok(mut slot) = ORIGINAL.lock() {
        *slot = Some((fd, original));
    }
    Some(RawGuard { _private: () })
}

/// Idempotent, and safe to call when raw mode was never entered.
pub fn restore() {
    let taken = ORIGINAL.lock().ok().and_then(|mut slot| slot.take());
    if let Some((fd, original)) = taken {
        unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, &original) };
    }
}

/// A panic anywhere in the shim must still hand the terminal back. Without this, one
/// unwrap in a reader thread costs the user their shell.
pub fn install_panic_restore() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_column_count_is_absent_not_zero() {
        assert_eq!(size_from_winsize(0, 0), None);
        assert_eq!(size_from_winsize(0, 50), None);
    }

    #[test]
    fn a_missing_row_count_falls_back_but_keeps_the_columns() {
        assert_eq!(size_from_winsize(200, 0), Some((200, DEFAULT_SIZE.1)));
    }

    #[test]
    fn a_real_size_is_passed_through() {
        assert_eq!(size_from_winsize(120, 40), Some((120, 40)));
    }

    #[test]
    fn size_never_returns_zero_even_with_no_terminal_anywhere() {
        // Under `cargo test` the std fds may or may not be terminals; either way the
        // contract is a usable size.
        let (cols, rows) = size();
        assert!(cols > 0 && rows > 0);
    }

    #[test]
    fn restore_is_safe_without_raw_mode() {
        restore();
        restore();
    }
}

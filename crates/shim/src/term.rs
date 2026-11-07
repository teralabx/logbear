//! Terminal size and raw mode.
//!
//! The pty handed to the child is sized to the user's real terminal, so a program that
//! reads `COLUMNS` does not hard-wrap at 80 inside a 200-column window.
//!
//! Raw mode stops our own tty interpreting keystrokes — no echo, no line buffering, no
//! local signal generation — leaving the pty's line discipline as the only one in play. A
//! process that exits without restoring it leaves the user with an invisible, unresponsive
//! shell, so the original termios is stashed in a global as soon as it is taken and every
//! exit path — normal return, guard drop, panic hook, `process::exit` — goes through
//! [`restore`].

use std::os::fd::RawFd;
use std::sync::Mutex;

/// Fallback size when nothing reports one, the same one `stty` uses.
pub const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// Interprets a `winsize`, treating a zero column count as absent.
///
/// A zero is the kernel saying "I don't know" — what a pipe, a cron job or a CI runner
/// reports — not a zero-width terminal.
pub fn size_from_winsize(cols: u16, rows: u16) -> Option<(u16, u16)> {
    if cols == 0 {
        return None;
    }
    Some((cols, if rows == 0 { DEFAULT_SIZE.1 } else { rows }))
}

/// Terminal size from the first standard fd that knows it, or [`DEFAULT_SIZE`].
///
/// stdin is asked first: it is the fd still attached to the terminal when stdout has been
/// redirected to a file.
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

/// Pushes a size onto the pty master.
///
/// Uses a raw ioctl rather than `MasterPty` because the signal-watching thread needs it and
/// the master handle is not `Sync`.
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

/// Global because the restore has to be reachable from the panic hook and the exit path as
/// well as from the guard's `Drop`.
static ORIGINAL: Mutex<Option<(RawFd, libc::termios)>> = Mutex::new(None);

/// Restores the terminal when dropped.
///
/// The explicit [`restore`] call on the exit path exists because `process::exit` skips
/// drops.
pub struct RawGuard {
    _private: (),
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Puts `fd` into raw mode.
///
/// Returns `None` when `fd` is not a terminal — the normal case under CI or a pipe, where
/// there is no line discipline of ours to get out of the way.
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
    // Return from read() on a single byte: a keystroke has to reach the child now, not
    // once a buffer fills.
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

/// Restores the saved terminal state; idempotent, and safe when raw mode was never entered.
pub fn restore() {
    let taken = ORIGINAL.lock().ok().and_then(|mut slot| slot.take());
    if let Some((fd, original)) = taken {
        // Not TCSADRAIN, which waits for the terminal to take everything written so far: a
        // terminal that has stopped reading would then hold the exit forever.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
    }
}

/// Whether this process is in the foreground of the terminal on `fd`.
///
/// A background job that touches the terminal's settings or reads from it is stopped by the
/// kernel, which is how `logbear server &` used to halt on the spot.
pub fn in_foreground(fd: RawFd) -> bool {
    let group = unsafe { libc::tcgetpgrp(fd) };
    group > 0 && group == unsafe { libc::getpgrp() }
}

/// Installs a panic hook that restores the terminal before the previous hook runs.
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
        let (cols, rows) = size();
        assert!(cols > 0 && rows > 0);
    }

    #[test]
    fn restore_is_safe_without_raw_mode() {
        restore();
        restore();
    }
}

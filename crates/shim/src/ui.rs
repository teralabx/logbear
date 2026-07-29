//! Terminal presentation for the query subcommands.
//!
//! Colour is emitted only when stdout is a tty, so piping into `grep` or a file produces
//! clean text.

use std::sync::OnceLock;

/// The brand palette as truecolor SGR, matching the app: `flare` is the error red-orange,
/// `clear` the success green, `ash` the metadata grey.
pub const FLARE: &str = "\x1b[38;2;229;83;61m";
pub const CLEAR: &str = "\x1b[38;2;79;157;119m";
pub const ASH: &str = "\x1b[38;2;138;145;153m";
/// Warnings: between flare and ash on the same hue arc.
pub const EMBER: &str = "\x1b[38;2;217;154;61m";
pub const BOLD: &str = "\x1b[1m";
pub const RESET: &str = "\x1b[0m";

static COLOUR: OnceLock<bool> = OnceLock::new();

pub fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

/// Honours `NO_COLOR` and `LOGBEAR_COLOR=always|never` before falling back to the tty test.
pub fn colour_enabled() -> bool {
    *COLOUR.get_or_init(|| match std::env::var("LOGBEAR_COLOR").ok().as_deref() {
        Some("always") => true,
        Some("never") => false,
        _ => std::env::var_os("NO_COLOR").is_none() && is_tty(libc::STDOUT_FILENO),
    })
}

pub fn paint(colour: &str, text: &str) -> String {
    if colour_enabled() {
        format!("{colour}{text}{RESET}")
    } else {
        text.to_string()
    }
}

pub fn flare(t: &str) -> String {
    paint(FLARE, t)
}
pub fn clear(t: &str) -> String {
    paint(CLEAR, t)
}
pub fn ash(t: &str) -> String {
    paint(ASH, t)
}
pub fn bold(t: &str) -> String {
    paint(BOLD, t)
}

pub fn level_colour(level: logbear_core::Level) -> &'static str {
    use logbear_core::Level::*;
    match level {
        Error => FLARE,
        Warn => EMBER,
        Info => "",
        Debug | Trace | Unknown => ASH,
    }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// A relative time, like "4m ago".
pub fn rel_time(micros: i64) -> String {
    rel_time_from(logbear_core::now_micros(), micros)
}

pub fn rel_time_from(now: i64, then: i64) -> String {
    let secs = (now - then) / 1_000_000;
    match secs {
        s if s < 0 => "just now".into(),
        s if s < 10 => "just now".into(),
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 86_400 * 14 => format!("{}d ago", s / 86_400),
        s => format!("{}w ago", s / (86_400 * 7)),
    }
}

/// A wall-clock duration, at a precision suited to its magnitude.
pub fn duration(micros: i64) -> String {
    let ms = micros / 1000;
    match ms {
        m if m < 0 => "-".into(),
        m if m < 1000 => format!("{m}ms"),
        m if m < 60_000 => format!("{:.1}s", m as f64 / 1000.0),
        m if m < 3_600_000 => format!("{}m{:02}s", m / 60_000, (m % 60_000) / 1000),
        m => format!("{}h{:02}m", m / 3_600_000, (m % 3_600_000) / 60_000),
    }
}

pub fn local_hms(micros: i64) -> String {
    with_tm(micros, |tm| {
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    })
}

/// `localtime_r` rather than a date crate: only the user's local wall clock is needed, and
/// libc already knows it.
fn with_tm(micros: i64, f: impl FnOnce(&libc::tm) -> String) -> String {
    let secs = micros.div_euclid(1_000_000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if ok {
        f(&tm)
    } else {
        "--:--:--".into()
    }
}

/// `10m`, `2h`, `1d`, `45s`, or a bare number meaning minutes.
pub fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (digits, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&s[..i], &s[i..]),
        None => (s, "m"),
    };
    let n: u64 = digits.parse().ok()?;
    let secs = match unit.trim() {
        "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n * 60,
        "h" | "hr" | "hrs" => n * 3600,
        "d" | "day" | "days" => n * 86_400,
        "w" | "week" | "weeks" => n * 604_800,
        _ => return None,
    };
    Some(std::time::Duration::from_secs(secs))
}

pub fn short_sha(sha: Option<&String>) -> String {
    match sha {
        Some(s) if s.len() >= 7 => s[..7].to_string(),
        Some(s) => s.clone(),
        None => "-".into(),
    }
}

pub fn bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

/// A table cell.
///
/// Text and colour are kept apart because column widths are computed on what the eye sees,
/// and an SGR sequence is eleven invisible characters wide.
pub struct Cell {
    pub text: String,
    pub colour: Option<&'static str>,
}

impl Cell {
    pub fn plain(text: impl Into<String>) -> Cell {
        Cell {
            text: text.into(),
            colour: None,
        }
    }

    pub fn coloured(text: impl Into<String>, colour: &'static str) -> Cell {
        Cell {
            text: text.into(),
            colour: if colour.is_empty() { None } else { Some(colour) },
        }
    }

    fn width(&self) -> usize {
        self.text.chars().count()
    }

    /// Colour without padding, for a label/value layout rather than a table.
    pub fn render_alone(&self) -> String {
        match self.colour {
            Some(c) if colour_enabled() => format!("{c}{}{RESET}", self.text),
            _ => self.text.clone(),
        }
    }

    fn render(&self, width: usize, last: bool) -> String {
        let pad = width.saturating_sub(self.width());
        let body = match self.colour {
            Some(c) if colour_enabled() => format!("{c}{}{RESET}", self.text),
            _ => self.text.clone(),
        };
        if last {
            body
        } else {
            format!("{body}{}", " ".repeat(pad))
        }
    }
}

pub fn table(headers: &[&str], rows: &[Vec<Cell>]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.width());
        }
    }

    let mut out = String::new();
    let header: String = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let pad = widths[i].saturating_sub(h.chars().count());
            if i + 1 == cols {
                h.to_string()
            } else {
                format!("{h}{}  ", " ".repeat(pad))
            }
        })
        .collect();
    out.push_str(&ash(header.trim_end()));
    out.push('\n');

    let empty = Cell::plain("");
    for row in rows {
        let mut line = String::new();
        for (i, width) in widths.iter().enumerate() {
            let cell = row.get(i).unwrap_or(&empty);
            line.push_str(&cell.render(*width, i + 1 == cols));
            if i + 1 != cols {
                line.push_str("  ");
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The message every subcommand prints when the daemon is not running.
pub fn daemon_down(socket: &std::path::Path) -> String {
    format!(
        "logbear: the daemon is not running (no socket at {}).\n       start it with `logbeard serve`, or open the logbear app.",
        socket.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_read_like_english() {
        let now = 1_000_000_000_000_000;
        assert_eq!(rel_time_from(now, now), "just now");
        assert_eq!(rel_time_from(now, now - 40_000_000), "40s ago");
        assert_eq!(rel_time_from(now, now - 4 * 60 * 1_000_000), "4m ago");
        assert_eq!(rel_time_from(now, now - 3 * 3600 * 1_000_000), "3h ago");
        assert_eq!(rel_time_from(now, now - 2 * 86_400 * 1_000_000), "2d ago");
    }

    #[test]
    fn durations_scale_to_magnitude() {
        assert_eq!(duration(12_000), "12ms");
        assert_eq!(duration(1_500_000), "1.5s");
        assert_eq!(duration(72_000_000), "1m12s");
        assert_eq!(duration(3_900_000_000), "1h05m");
    }

    #[test]
    fn durations_parse() {
        use std::time::Duration;
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86_400)));
        assert_eq!(parse_duration("15"), Some(Duration::from_secs(900)));
        assert_eq!(parse_duration("soon"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn short_sha_survives_a_missing_repo() {
        assert_eq!(short_sha(Some(&"a1b2c3d4e5f6".to_string())), "a1b2c3d");
        assert_eq!(short_sha(None), "-");
    }

    #[test]
    fn table_columns_align_on_the_widest_cell() {
        let rows = vec![
            vec![Cell::plain("a"), Cell::plain("1")],
            vec![Cell::plain("longer"), Cell::plain("2")],
        ];
        let out = table(&["NAME", "N"], &rows);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], "a       1");
        assert_eq!(lines[2], "longer  2");
    }

    #[test]
    fn byte_sizes_are_human() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KB");
        assert_eq!(bytes(1024 * 1024 * 1024), "1.0 GB");
    }
}

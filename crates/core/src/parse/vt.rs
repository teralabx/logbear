//! Terminal emulation, not escape-stripping.
//!
//! A spinner or a build progress bar rewrites one visual line in place with `\r` and
//! `ESC[2K`, spending thousands of bytes to show one thing. Regex-stripping the escapes
//! turns that into thousands of junk rows. Running the bytes through a real VT state
//! machine collapses it to its final state, which is what the human saw.
//!
//! Two deliberate departures from a real terminal:
//!
//! * **No wrapping.** A terminal soft-wraps at the right margin without inserting any
//!   bytes, so re-emulating at unbounded width recovers the unwrapped logical line. If we
//!   wrapped at the user's real width, the same error would fingerprint differently on a
//!   laptop and on an external monitor.
//! * **One row.** Input is a single raw line (no LF), so vertical cursor motion has
//!   nowhere to go and is ignored.

use crate::model::CANONICAL_COLS;

/// Render one raw line's bytes to plain text: ANSI resolved, `\r` overwrites applied,
/// trailing whitespace trimmed.
pub fn render_line(bytes: &[u8]) -> String {
    let mut perf = Row::default();
    let mut parser = vte::Parser::new();
    parser.advance(&mut perf, bytes);
    perf.finish()
}

/// Render a sequence of raw lines as a block of text.
pub fn render_lines<'a, I>(lines: I) -> String
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut out = String::new();
    for (i, line) in lines.into_iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&render_line(line));
    }
    out
}

/// Strip escape sequences without emulating. Only for cheap prefilters — mode detection
/// peeking at a line start — never for stored text.
pub fn quick_strip(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1;
            } else if i < bytes.len() && bytes[i] == b']' {
                while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                    i += 1;
                }
                i += 1;
            } else {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

#[derive(Default)]
struct Row {
    cells: Vec<char>,
    cursor: usize,
}

impl Row {
    fn put(&mut self, c: char) {
        if self.cells.len() <= self.cursor {
            self.cells.resize(self.cursor + 1, ' ');
        }
        self.cells[self.cursor] = c;
        self.cursor += 1;
    }

    fn finish(self) -> String {
        let mut s: String = self.cells.into_iter().collect();
        let trimmed = s.trim_end();
        s.truncate(trimmed.len());
        s
    }
}

impl vte::Perform for Row {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.cursor = 0,
            0x08 => self.cursor = self.cursor.saturating_sub(1),
            b'\t' => {
                let next = (self.cursor / 8 + 1) * 8;
                while self.cursor < next {
                    self.put(' ');
                }
            }
            // Bell, and anything else a single line cannot act on.
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, _inter: &[u8], _ignore: bool, action: char) {
        let p = |n: usize, default: usize| -> usize {
            params
                .iter()
                .nth(n)
                .and_then(|s| s.first().copied())
                .map(|v| v as usize)
                .filter(|v| *v != 0)
                .unwrap_or(default)
        };
        match action {
            // Erase in line
            'K' => {
                let mode = params
                    .iter()
                    .next()
                    .and_then(|s| s.first().copied())
                    .unwrap_or(0);
                match mode {
                    0 => self.cells.truncate(self.cursor),
                    1 => {
                        for i in 0..self.cursor.min(self.cells.len()) {
                            self.cells[i] = ' ';
                        }
                    }
                    _ => self.cells.clear(),
                }
            }
            // Erase in display — on a single row, same as clearing it.
            'J' => {
                self.cells.clear();
                self.cursor = 0;
            }
            // Cursor horizontal absolute
            'G' | '`' => self.cursor = p(0, 1) - 1,
            // Cursor forward / back
            'C' => self.cursor += p(0, 1),
            'D' => self.cursor = self.cursor.saturating_sub(p(0, 1)),
            // Cursor position — only the column is meaningful here.
            'H' | 'f' => self.cursor = p(1, 1) - 1,
            // Delete characters
            'P' => {
                let n = p(0, 1);
                let end = (self.cursor + n).min(self.cells.len());
                if self.cursor < self.cells.len() {
                    self.cells.drain(self.cursor..end);
                }
            }
            // Insert blanks
            '@' => {
                let n = p(0, 1);
                if self.cursor <= self.cells.len() {
                    for _ in 0..n {
                        self.cells.insert(self.cursor, ' ');
                    }
                }
            }
            // SGR and everything else does not change which characters are on the row.
            _ => {}
        }
    }

    fn hook(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
    /// Window titles and hyperlink wrappers must not bleed into stored text.
    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {}
    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {}
}

/// The width the stored projection is emulated at, recorded for the record's sake even
/// though nothing wraps at it.
pub const fn canonical_cols() -> usize {
    CANONICAL_COLS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(render_line(b"hello world"), "hello world");
    }

    #[test]
    fn colour_is_resolved_away() {
        let s = render_line(b"\x1b[31m** (RuntimeError) boom\x1b[0m");
        assert_eq!(s, "** (RuntimeError) boom");
    }

    #[test]
    fn carriage_return_overwrites_in_place() {
        // The shape a progress bar actually makes.
        let bytes = b"[    ] 0%\r[==  ] 50%\r[====] 100%";
        assert_eq!(render_line(bytes), "[====] 100%");
    }

    #[test]
    fn erase_line_then_rewrite() {
        let bytes = b"downloading foo\r\x1b[2Kdone";
        assert_eq!(render_line(bytes), "done");
    }

    #[test]
    fn partial_overwrite_keeps_the_tail() {
        // Real terminals leave the uncovered remainder visible; so must we.
        assert_eq!(render_line(b"abcdef\rXY"), "XYcdef");
    }

    #[test]
    fn backspace_erases_by_overwrite() {
        assert_eq!(render_line(b"abc\x08\x08X"), "aXc");
    }

    #[test]
    fn tabs_expand_to_the_next_stop() {
        assert_eq!(render_line(b"a\tb"), "a       b");
    }

    #[test]
    fn osc_window_titles_do_not_leak() {
        let s = render_line(b"\x1b]0;my terminal title\x07real output");
        assert_eq!(s, "real output");
    }

    #[test]
    fn nothing_wraps_at_the_canonical_width() {
        let long = "x".repeat(CANONICAL_COLS * 3);
        assert_eq!(render_line(long.as_bytes()).len(), CANONICAL_COLS * 3);
    }

    #[test]
    fn trailing_whitespace_is_trimmed_but_leading_indent_is_not() {
        assert_eq!(render_line(b"    at Object.foo (a.js:1:1)   "), "    at Object.foo (a.js:1:1)");
    }

    #[test]
    fn rendering_is_deterministic() {
        let bytes = b"\x1b[32m12:04:11.882 \x1b[0m[info] GET /users\r\n";
        assert_eq!(render_line(bytes), render_line(bytes));
    }

    #[test]
    fn multiple_lines_join_with_newlines() {
        let lines: Vec<&[u8]> = vec![b"one", b"two"];
        assert_eq!(render_lines(lines), "one\ntwo");
    }
}

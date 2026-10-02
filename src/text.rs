//! Terminal text measured in display columns (CJK and most emoji take two).

use std::collections::VecDeque;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of `s` in terminal columns.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// `s` cut to at most `max` columns; the last column is `…` when cut. A double-width
/// character that does not fit is dropped whole.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// `s` padded with spaces on the right to `w` columns (unchanged when already wider).
pub fn pad(s: &str, w: usize) -> String {
    let mut out = s.to_string();
    out.extend(std::iter::repeat_n(' ', w.saturating_sub(width(s))));
    out
}

/// A token count for a narrow column: whole below 1,000, else in K, M, B or T rounded to one
/// decimal below 100 (a trailing `.0` dropped) and to a whole number from 100 (`1.2M`, `93.3B`,
/// `118K`). Rounding up moves to the next unit (`999_500` is `1M`).
pub fn human_count(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let n = u128::from(n);
    for (unit, suffix) in [
        (1_000, "K"),
        (1_000_000, "M"),
        (1_000_000_000, "B"),
        (1_000_000_000_000, "T"),
    ] {
        let tenths = (n * 10 + unit / 2) / unit;
        if tenths < 1000 {
            return match tenths % 10 {
                0 => format!("{}{suffix}", tenths / 10),
                d => format!("{}.{d}{suffix}", tenths / 10),
            };
        }
        let whole = (n + unit / 2) / unit;
        if whole < 1000 {
            return format!("{whole}{suffix}");
        }
    }
    let unit: u128 = 1_000_000_000_000;
    format!("{}T", (n + unit / 2) / unit)
}

/// A cost in picodollars (10⁻¹² USD) for a narrow column, rounded half up to the cent: below
/// $1,000 with two decimals (`$0.00`, `$0.42`, `$12.34`, `$999.99`), a nonzero cost below half a
/// cent as `<$0.01`, from $1,000 on as [`human_count`] of whole dollars (`$1K`, `$1.2K`, `$118K`).
pub fn human_usd(pico: u128) -> String {
    let cents = pico.saturating_add(5_000_000_000) / 10_000_000_000;
    if pico > 0 && cents == 0 {
        return "<$0.01".to_string();
    }
    if cents < 100_000 {
        return format!("${}.{:02}", cents / 100, cents % 100);
    }
    let dollars = pico.saturating_add(500_000_000_000) / 1_000_000_000_000;
    format!("${}", human_count(dollars.min(u128::from(u64::MAX)) as u64))
}

/// `s` wrapped to lines of at most `max` columns: at spaces where possible, inside a word
/// when the word alone is too wide. Existing line breaks are kept; tabs count as spaces.
pub fn wrap(s: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut out = Vec::new();
    for raw in s.split('\n') {
        let raw = raw.replace('\t', "    ");
        let mut line = String::new();
        let mut used = 0;
        for word in raw.split(' ') {
            let w = width(word);
            let sep = usize::from(!line.is_empty());
            if used + sep + w <= max {
                if sep == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                used += sep + w;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            for c in word.chars() {
                let cw = c.width().unwrap_or(0);
                if used + cw > max && !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += cw;
            }
        }
        out.push(line);
    }
    out
}

/// The virtual screen is at most this wide: the cursor stops at the last column, and a
/// character past it is dropped.
const MAX_COLS: usize = 1000;
/// And at most this tall: the cursor stops at the last row, and a line break there drops the
/// earliest row, so a long log keeps its end.
const MAX_ROWS: usize = 100_000;
/// The cells of all its rows together (that many rows of that many columns would be
/// gigabytes): beyond, the earliest rows are dropped.
const MAX_CELLS: usize = 2_000_000;

/// Raw terminal output (as `claude logs` prints it: colors, cursor moves, clear-screen) as
/// plain text: escape sequences are interpreted on a small virtual screen whose final
/// contents are returned, one line per row, trailing blanks trimmed. Anything not understood
/// is dropped, never shown as escape codes. The screen is bounded (`MAX_COLS`, `MAX_ROWS`,
/// `MAX_CELLS`), so no sequence, whatever its parameters, costs more than that.
pub fn terminal_text(raw: &str) -> String {
    let mut screen = VirtualScreen::default();
    screen.feed(raw);
    screen.text()
}

/// Rows of cells; a cell holds one character (plus combining marks); the cell after a
/// double-width character is an empty continuation. The cursor's row is below [`MAX_ROWS`]
/// and its column at most [`MAX_COLS`] (there, after a character in the last column).
#[derive(Default)]
struct VirtualScreen {
    rows: VecDeque<Vec<String>>,
    /// The cells of `rows`, counted as rows grow and shrink.
    cells: usize,
    row: usize,
    col: usize,
    saved: (usize, usize),
}

impl VirtualScreen {
    /// Draws `raw` on the screen.
    fn feed(&mut self, raw: &str) {
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let mut last = None;
                        for c in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&c) {
                                last = Some(c);
                                break;
                            }
                            params.push(c);
                        }
                        if let Some(command) = last {
                            self.csi(&params, command);
                        }
                    }
                    // OSC and the other string sequences run to BEL or ESC \.
                    Some(']' | 'P' | 'X' | '^' | '_') => {
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                            {
                                break;
                            }
                        }
                    }
                    Some('7') => self.saved = (self.row, self.col),
                    Some('8') => (self.row, self.col) = self.saved,
                    Some('M') => self.row = self.row.saturating_sub(1),
                    Some('c') => *self = VirtualScreen::default(),
                    // ESC, intermediate bytes, final byte (e.g. `ESC ( B`).
                    Some(' '..='/') => {
                        while chars.next_if(|c| (' '..='/').contains(c)).is_some() {}
                        chars.next();
                    }
                    _ => {}
                },
                '\n' => self.newline(),
                '\r' => self.col = 0,
                '\t' => self.move_col((self.col / 8 + 1) * 8),
                '\u{8}' => self.col = self.col.saturating_sub(1),
                c if c.is_control() => {}
                c => self.put(c),
            }
        }
    }

    fn line(&mut self) -> &mut Vec<String> {
        if self.rows.len() <= self.row {
            self.rows.resize(self.row + 1, Vec::new());
        }
        &mut self.rows[self.row]
    }

    /// Moves the cursor to `row`, or to the last row when it is past it.
    fn move_row(&mut self, row: usize) {
        self.row = row.min(MAX_ROWS - 1);
    }

    /// Moves the cursor to `col`, or to the last column when it is past it.
    fn move_col(&mut self, col: usize) {
        self.col = col.min(MAX_COLS - 1);
    }

    fn put(&mut self, c: char) {
        let w = c.width().unwrap_or(0);
        let col = self.col;
        if w == 0 {
            let line = self.line();
            if let Some(prev) = col.checked_sub(1).and_then(|i| line.get_mut(i)) {
                prev.push(c);
            }
            return;
        }
        // Past the last column: dropped.
        if col + w > MAX_COLS {
            return;
        }
        let line = self.line();
        let grown = (col + w).saturating_sub(line.len());
        if grown > 0 {
            line.resize(col + w, " ".to_string());
        }
        // Overwriting half of a double-width character blanks the other half.
        if line[col].is_empty() && col > 0 {
            line[col - 1] = " ".to_string();
        }
        if line.get(col + w).is_some_and(String::is_empty) {
            line[col + w] = " ".to_string();
        }
        line[col] = c.to_string();
        for cell in &mut line[col + 1..col + w] {
            cell.clear();
        }
        self.col += w;
        self.cells += grown;
        // Never the cursor's own row: the screen may hold one row's cells more.
        while self.cells > MAX_CELLS && self.row > 0 {
            self.scroll();
        }
    }

    /// Drops the earliest row; the cursor and the saved position stay on their rows.
    fn scroll(&mut self) {
        if let Some(gone) = self.rows.pop_front() {
            self.cells -= gone.len();
        }
        self.row = self.row.saturating_sub(1);
        self.saved.0 = self.saved.0.saturating_sub(1);
    }

    fn newline(&mut self) {
        if self.row + 1 == MAX_ROWS {
            self.line();
            self.scroll();
        }
        self.row += 1;
        self.col = 0;
        self.line();
    }

    /// Cuts the cursor's row to its first `len` cells.
    fn cut(&mut self, len: usize) {
        let line = self.line();
        let gone = line.len().saturating_sub(len);
        line.truncate(len);
        self.cells -= gone;
    }

    fn csi(&mut self, params: &str, command: char) {
        // A number too large to hold is the largest one: the cursor stops at the edge anyway.
        let number = |p: &str| {
            p.parse().unwrap_or_else(|_| {
                let digits = !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
                if digits { usize::MAX } else { 0 }
            })
        };
        let nums: Vec<usize> = params
            .trim_start_matches(['?', '<', '=', '>'])
            .split(';')
            .map(number)
            .collect();
        let n = |i: usize| nums.get(i).copied().filter(|&v| v > 0).unwrap_or(1);
        let raw = nums.first().copied().unwrap_or(0);
        match command {
            'A' => self.row = self.row.saturating_sub(n(0)),
            'B' => self.move_row(self.row.saturating_add(n(0))),
            'C' => self.move_col(self.col.saturating_add(n(0))),
            'D' => self.col = self.col.saturating_sub(n(0)),
            'E' => {
                self.move_row(self.row.saturating_add(n(0)));
                self.col = 0;
            }
            'F' => (self.row, self.col) = (self.row.saturating_sub(n(0)), 0),
            'G' => self.move_col(n(0) - 1),
            'd' => self.move_row(n(0) - 1),
            'H' | 'f' => {
                self.move_row(n(0) - 1);
                self.move_col(n(1) - 1);
            }
            's' => self.saved = (self.row, self.col),
            'u' => (self.row, self.col) = self.saved,
            'J' => match raw {
                0 => {
                    let (row, col) = (self.row, self.col);
                    let below: usize = self.rows.iter().skip(row + 1).map(Vec::len).sum();
                    self.rows.truncate(row + 1);
                    self.cells -= below;
                    self.cut(col);
                }
                1 => {
                    let (row, col) = (self.row, self.col);
                    for line in self.rows.iter_mut().take(row) {
                        self.cells -= line.len();
                        line.clear();
                    }
                    let line = self.line();
                    for cell in line.iter_mut().take(col + 1) {
                        *cell = " ".to_string();
                    }
                }
                _ => {
                    self.rows.clear();
                    self.cells = 0;
                }
            },
            'K' => match raw {
                0 => self.cut(self.col),
                1 => {
                    let col = self.col;
                    for cell in self.line().iter_mut().take(col + 1) {
                        *cell = " ".to_string();
                    }
                }
                _ => self.cut(0),
            },
            _ => {}
        }
    }

    fn text(&self) -> String {
        let mut lines: Vec<String> = self
            .rows
            .iter()
            .map(|cells| cells.concat().trim_end().to_string())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// The bounds hold, and `cells` is what the rows hold.
    #[cfg(test)]
    fn check(&self) {
        assert!(self.rows.len() <= MAX_ROWS, "{} rows", self.rows.len());
        assert!(self.row < MAX_ROWS && self.col <= MAX_COLS);
        assert!(self.saved.0 < MAX_ROWS && self.saved.1 <= MAX_COLS);
        assert!(self.rows.iter().all(|line| line.len() <= MAX_COLS));
        assert_eq!(self.cells, self.rows.iter().map(Vec::len).sum::<usize>());
        assert!(self.cells <= MAX_CELLS + MAX_COLS, "{} cells", self.cells);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_counts_columns() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本語"), 6);
        assert_eq!(width("a日b"), 4);
        assert_eq!(width(""), 0);
    }

    #[test]
    fn truncate_by_columns() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abcd", 4), "abcd");
        assert_eq!(truncate("日本語テキスト", 14), "日本語テキスト");
        assert_eq!(truncate("日本語テキスト", 7), "日本語…");
        // A double-width character never straddles the limit.
        assert_eq!(truncate("日本語テキスト", 6), "日本…");
        assert_eq!(width(&truncate("日本語テキスト", 6)), 5);
        assert_eq!(truncate("abc", 1), "…");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn wrap_by_columns() {
        assert_eq!(wrap("the quick brown fox", 9), ["the quick", "brown fox"]);
        assert_eq!(wrap("a\n\nb", 5), ["a", "", "b"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap("日本語テキスト", 5), ["日本", "語テ", "キス", "ト"]);
        assert_eq!(wrap("x 日本語", 6), ["x", "日本語"]);
        assert_eq!(wrap("", 4), [""]);
        assert!(
            wrap("word ".repeat(30).trim(), 20)
                .iter()
                .all(|l| width(l) <= 20)
        );
    }

    #[test]
    fn terminal_output_becomes_plain_text() {
        let esc = "\u{1b}";
        // Colors and modes vanish; CRLF and lone CR end lines; tabs become spaces.
        assert_eq!(
            terminal_text(&format!(
                "{esc}[?25l{esc}[1;32mok{esc}[0m done\r\nnext\tcol\rover\n"
            )),
            "ok done\nover    col"
        );
        // Clearing the screen drops what was drawn before; absolute moves place text.
        assert_eq!(
            terminal_text(&format!(
                "old frame\r\n{esc}[2J{esc}[H{esc}[3;5Hthird{esc}[1;1Hfirst{esc}[2;1Hsecond"
            )),
            "first\nsecond\n    third"
        );
        // Relative moves and erasing, as ink redraws a frame in place.
        assert_eq!(
            terminal_text(&format!(
                "working 1\r\nstatus a\r\n{esc}[2K{esc}[1A{esc}[2K{esc}[1A{esc}[Gworking 2\r\nstatus b\r\n"
            )),
            "working 2\nstatus b"
        );
        assert_eq!(
            terminal_text(&format!("a{esc}[3Cb{esc}[2Dc{esc}[Kd")),
            "a  cd"
        );
        // OSC (window title, hyperlinks) with either terminator, and other escapes.
        assert_eq!(
            terminal_text(&format!(
                "{esc}]0;title\u{7}{esc}]8;;http://x{esc}\\link{esc}]8;;{esc}\\{esc}(B{esc}=!"
            )),
            "link!"
        );
        // Double-width text keeps its columns; stray control bytes are dropped.
        assert_eq!(terminal_text("日本\u{8}\u{7}x"), "日 x");
        assert_eq!(terminal_text(""), "");
        // A cut-off sequence at the end is ignored.
        assert_eq!(terminal_text(&format!("end{esc}[3")), "end");
    }

    /// R7: a cursor parameter of any size costs nothing: the screen is bounded, so 20 bytes
    /// cannot ask for terabytes (an allocation failure aborts, and the terminal stays raw).
    #[test]
    fn huge_cursor_parameters_stay_bounded() {
        let esc = "\u{1b}";
        // Rows: the cursor stops at the last row.
        for row in ["9999999999999", "20000000", "99999999999999999999999999"] {
            let out = terminal_text(&format!("{esc}[{row};1H x"));
            assert_eq!(out.lines().count(), MAX_ROWS, "{row}");
            assert_eq!(out.len(), MAX_ROWS + 1, "{row}");
            assert!(out.ends_with("\n x"), "{row}");
        }
        // Columns: the cursor stops at the last column, and what is past it is dropped.
        assert_eq!(terminal_text(&format!("{esc}[10000000C x")), "");
        let out = terminal_text(&format!("a{esc}[10000000Cb{esc}[99999999999999999999Gcd"));
        assert_eq!(width(&out), MAX_COLS);
        assert!(out.starts_with('a') && out.ends_with('c'), "{out:?}");
        let out = terminal_text(&"\t".repeat(5000));
        assert_eq!(out, "");
        let out = terminal_text(&"x".repeat(MAX_COLS + 50));
        assert_eq!(out, "x".repeat(MAX_COLS));
        // A double-width character that does not fit in the last column is dropped whole.
        let out = terminal_text(&format!("{esc}[{MAX_COLS}G日x"));
        assert_eq!(width(&out), MAX_COLS);
        assert!(out.ends_with('x'), "{out:?}");
    }

    /// R7: no arithmetic on the cursor overflows (a debug build would panic).
    #[test]
    fn cursor_arithmetic_saturates() {
        let esc = "\u{1b}";
        let max = usize::MAX;
        for command in ['A', 'B', 'C', 'D', 'E', 'F', 'G', 'd'] {
            let out = terminal_text(&format!(
                "{esc}[{max}{command}{esc}[{max}{command}x\r\n{esc}[{max}{command}y"
            ));
            assert!(out.contains('x') || out.contains('y'), "{command}: {out:?}");
        }
        for command in ['H', 'f'] {
            let out = terminal_text(&format!(
                "{esc}[{max};{max}{command}x\r\n\ty{esc}[{max}B{esc}[{max}Cz{esc}7{esc}8w"
            ));
            assert!(out.lines().count() <= MAX_ROWS, "{command}");
        }
        // Erasing from there, and saving and restoring it.
        let out = terminal_text(&format!(
            "a\r\nb{esc}[{max};{max}H{esc}[s{esc}[1J{esc}[1K{esc}[J{esc}[K{esc}[u{esc}[2Kc"
        ));
        assert!(out.ends_with('c'), "{out:?}");
    }

    /// R7: a line break at the last row drops the earliest row, so a long log keeps its end.
    #[test]
    fn the_screen_scrolls_at_its_last_row() {
        let raw: String = (0..MAX_ROWS + 5).map(|i| format!("line {i}\r\n")).collect();
        let out = terminal_text(&raw);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), MAX_ROWS - 1);
        assert_eq!(lines[0], "line 6");
        assert_eq!(lines[lines.len() - 1], format!("line {}", MAX_ROWS + 4));
        // A saved position stays on its row.
        let esc = "\u{1b}";
        let out = terminal_text(&format!("{esc}[{MAX_ROWS};1Ha{esc}7\r\nb{esc}8c"));
        assert!(out.ends_with("\nac\nb"), "{:?}", &out[out.len() - 8..]);
    }

    /// R7: the cells of all rows are bounded too (a hundred thousand rows of a thousand
    /// columns would be gigabytes): beyond, the earliest rows are dropped.
    #[test]
    fn the_screen_holds_a_bounded_number_of_cells() {
        let esc = "\u{1b}";
        let rows = MAX_CELLS / MAX_COLS + 500;
        let raw: String = (0..rows)
            .map(|i| format!("{i}{esc}[{MAX_COLS}G|\r\n"))
            .collect();
        let mut screen = VirtualScreen::default();
        screen.feed(&raw);
        screen.check();
        assert!(screen.cells <= MAX_CELLS + MAX_COLS, "{}", screen.cells);
        let out = screen.text();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), MAX_CELLS / MAX_COLS);
        assert!(lines[0].starts_with("500 "), "{:?}", &lines[0][..10]);
        assert!(lines[lines.len() - 1].starts_with(&format!("{} ", rows - 1)));
        assert!(
            lines
                .iter()
                .all(|l| width(l) == MAX_COLS && l.ends_with('|'))
        );
    }

    /// R7: whatever the sequences, the screen keeps within its bounds and its count of cells
    /// is exact.
    #[test]
    fn random_sequences_keep_the_screen_within_its_bounds() {
        // A small generator (xorshift): the same sequences on every run.
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let number = |r: u64, pick: u64| match pick {
            0 => String::new(),
            1 => "0".to_string(),
            2 => usize::MAX.to_string(),
            3 => "99999999999999999999999".to_string(),
            4 => (MAX_COLS as u64 - 2 + r % 5).to_string(),
            5 => (MAX_ROWS as u64 - 2 + r % 5).to_string(),
            6 => (r % 3).to_string(),
            _ => (r % 40).to_string(),
        };
        for _ in 0..40 {
            let mut screen = VirtualScreen::default();
            for step in 0..200 {
                let piece = match next(48) {
                    0 => "\u{1b}c".to_string(),
                    1..=4 => "\r\n".to_string(),
                    5..=8 => "\t".to_string(),
                    9..=12 => "日本".to_string(),
                    13..=16 => "e\u{301}\u{8}".to_string(),
                    17..=20 => ["\u{1b}7", "\u{1b}8", "\u{1b}M"][next(3) as usize].to_string(),
                    21..=36 => {
                        let command = b"ABCDEFGdHfsuJK"[next(14) as usize] as char;
                        let (a, b) = (number(next(1000), next(9)), number(next(1000), next(9)));
                        format!("\u{1b}[{a};{b}{command}")
                    }
                    _ => "word ".to_string(),
                };
                screen.feed(&piece);
                if step % 20 == 19 {
                    screen.check();
                }
            }
        }
    }

    #[test]
    fn human_count_rounds_to_units() {
        for (n, shown) in [
            (0, "0"),
            (999, "999"),
            (1000, "1K"),
            (1049, "1K"),
            (1050, "1.1K"),
            (99_949, "99.9K"),
            (99_950, "100K"),
            (999_499, "999K"),
            (999_500, "1M"),
            (1_234_567, "1.2M"),
            (93_328_770_717, "93.3B"),
            (u64::MAX, "18446744T"),
        ] {
            assert_eq!(human_count(n), shown, "{n}");
        }
    }

    #[test]
    fn human_usd_rounds_to_cents_then_units() {
        for (pico, shown) in [
            (0, "$0.00"),
            (1, "<$0.01"),
            (4_999_999_999, "<$0.01"),
            (5_000_000_000, "$0.01"),
            (420_000_000_000, "$0.42"),
            (12_344_999_999_999, "$12.34"),
            (999_994_999_999_999, "$999.99"),
            (999_995_000_000_000, "$1K"),
            (1_234_000_000_000_000, "$1.2K"),
            (45_600_000_000_000_000, "$45.6K"),
            (118_000_000_000_000_000, "$118K"),
            (1_200_000_000_000_000_000, "$1.2M"),
            (u128::MAX, "$18446744T"),
        ] {
            assert_eq!(human_usd(pico), shown, "{pico}");
        }
    }

    #[test]
    fn pad_by_columns() {
        assert_eq!(pad("日本", 6), "日本  ");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("abcdef", 4), "abcdef");
    }
}

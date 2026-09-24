//! Terminal text measured in display columns (CJK and most emoji take two).

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

/// Raw terminal output (as `claude logs` prints it: colors, cursor moves, clear-screen) as
/// plain text: escape sequences are interpreted on a small virtual screen whose final
/// contents are returned, one line per row, trailing blanks trimmed. Anything not understood
/// is dropped, never shown as escape codes.
pub fn terminal_text(raw: &str) -> String {
    let mut screen = VirtualScreen::default();
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
                        screen.csi(&params, command);
                    }
                }
                // OSC and the other string sequences run to BEL or ESC \.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                Some('7') => screen.saved = (screen.row, screen.col),
                Some('8') => (screen.row, screen.col) = screen.saved,
                Some('M') => screen.row = screen.row.saturating_sub(1),
                Some('c') => screen = VirtualScreen::default(),
                // ESC, intermediate bytes, final byte (e.g. `ESC ( B`).
                Some(' '..='/') => {
                    while chars.next_if(|c| (' '..='/').contains(c)).is_some() {}
                    chars.next();
                }
                _ => {}
            },
            '\n' => screen.newline(),
            '\r' => screen.col = 0,
            '\t' => screen.col = (screen.col / 8 + 1) * 8,
            '\u{8}' => screen.col = screen.col.saturating_sub(1),
            c if c.is_control() => {}
            c => screen.put(c),
        }
    }
    screen.text()
}

/// Rows of cells; a cell holds one character (plus combining marks); the cell after a
/// double-width character is an empty continuation.
#[derive(Default)]
struct VirtualScreen {
    rows: Vec<Vec<String>>,
    row: usize,
    col: usize,
    saved: (usize, usize),
}

impl VirtualScreen {
    fn line(&mut self) -> &mut Vec<String> {
        if self.rows.len() <= self.row {
            self.rows.resize(self.row + 1, Vec::new());
        }
        &mut self.rows[self.row]
    }

    fn put(&mut self, c: char) {
        let w = c.width().unwrap_or(0);
        let col = self.col;
        let line = self.line();
        if w == 0 {
            if let Some(prev) = col.checked_sub(1).and_then(|i| line.get_mut(i)) {
                prev.push(c);
            }
            return;
        }
        if line.len() < col + w {
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
    }

    fn newline(&mut self) {
        self.row += 1;
        self.col = 0;
        self.line();
    }

    fn csi(&mut self, params: &str, command: char) {
        let nums: Vec<usize> = params
            .trim_start_matches(['?', '<', '=', '>'])
            .split(';')
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        let n = |i: usize| nums.get(i).copied().filter(|&v| v > 0).unwrap_or(1);
        let raw = nums.first().copied().unwrap_or(0);
        match command {
            'A' => self.row = self.row.saturating_sub(n(0)),
            'B' => self.row += n(0),
            'C' => self.col += n(0),
            'D' => self.col = self.col.saturating_sub(n(0)),
            'E' => (self.row, self.col) = (self.row + n(0), 0),
            'F' => (self.row, self.col) = (self.row.saturating_sub(n(0)), 0),
            'G' => self.col = n(0) - 1,
            'd' => self.row = n(0) - 1,
            'H' | 'f' => (self.row, self.col) = (n(0) - 1, n(1) - 1),
            's' => self.saved = (self.row, self.col),
            'u' => (self.row, self.col) = self.saved,
            'J' => match raw {
                0 => {
                    let (row, col) = (self.row, self.col);
                    self.rows.truncate(row + 1);
                    self.line().truncate(col);
                }
                1 => {
                    let (row, col) = (self.row, self.col);
                    for line in self.rows.iter_mut().take(row) {
                        line.clear();
                    }
                    let line = self.line();
                    for cell in line.iter_mut().take(col + 1) {
                        *cell = " ".to_string();
                    }
                }
                _ => self.rows.clear(),
            },
            'K' => {
                let col = self.col;
                let line = self.line();
                match raw {
                    0 => line.truncate(col),
                    1 => {
                        for cell in line.iter_mut().take(col + 1) {
                            *cell = " ".to_string();
                        }
                    }
                    _ => line.clear(),
                }
            }
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

    #[test]
    fn pad_by_columns() {
        assert_eq!(pad("日本", 6), "日本  ");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("abcdef", 4), "abcdef");
    }
}

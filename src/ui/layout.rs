//! Width-aware text layout: ANSI stripping, wrapping, and column grids.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const GAP: usize = 2;
const MIN_TEXT: usize = 16;

pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(strip_ansi(text).as_str())
}

pub fn pad(text: &str, target: usize) -> String {
    let fill = target.saturating_sub(width(text));
    format!("{text}{}", " ".repeat(fill))
}

pub fn pad_left(text: &str, target: usize) -> String {
    let fill = target.saturating_sub(width(text));
    format!("{}{text}", " ".repeat(fill))
}

pub fn keep_tail(text: &str, max: usize, ellipsis: &str) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let room = max.saturating_sub(width(ellipsis));
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > room {
            break;
        }
        used += ch_width;
        tail.push(ch);
    }
    tail.reverse();
    format!("{ellipsis}{}", tail.into_iter().collect::<String>())
}

pub fn wrap(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split_whitespace() {
        for piece in split_long(word, limit) {
            let piece_width = UnicodeWidthStr::width(piece.as_str());
            if used > 0 && used + 1 + piece_width > limit {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            line.push_str(&piece);
            used += piece_width;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn split_long(word: &str, limit: usize) -> Vec<String> {
    if UnicodeWidthStr::width(word) <= limit {
        return vec![word.to_string()];
    }
    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut used = 0;
    for ch in word.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > limit && !piece.is_empty() {
            pieces.push(std::mem::take(&mut piece));
            used = 0;
        }
        piece.push(ch);
        used += ch_width;
    }
    if !piece.is_empty() {
        pieces.push(piece);
    }
    pieces
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    indent: usize,
    widths: Vec<usize>,
}

impl Grid {
    pub fn fit<'a>(indent: usize, rows: impl IntoIterator<Item = &'a [String]>) -> Self {
        let mut widths: Vec<usize> = Vec::new();
        for row in rows {
            if widths.len() < row.len() {
                widths.resize(row.len(), 0);
            }
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(width(cell));
            }
        }
        Self { indent, widths }
    }

    pub fn column_start(&self, index: usize) -> usize {
        let index = index.min(self.widths.len());
        self.indent
            + self.widths[..index]
                .iter()
                .map(|width| width + GAP)
                .sum::<usize>()
    }

    pub fn text_column(&self) -> usize {
        self.column_start(self.widths.len())
    }

    pub fn align_column_to(&mut self, index: usize, column: usize) {
        let current = self.column_start(index);
        if index > 0 && index <= self.widths.len() && column > current {
            self.widths[index - 1] += column - current;
        }
    }

    pub fn align_text_to(&mut self, column: usize) {
        let current = self.text_column();
        if column > current
            && let Some(last) = self.widths.last_mut()
        {
            *last += column - current;
        }
    }

    fn lead(&self, cells: &[String]) -> String {
        let mut out = " ".repeat(self.indent);
        for (index, target) in self.widths.iter().enumerate() {
            let cell = cells.get(index).map(String::as_str).unwrap_or("");
            out.push_str(&pad(cell, *target));
            out.push_str(&" ".repeat(GAP));
        }
        out
    }

    pub fn row(&self, cells: &[String], value: &str) -> String {
        let mut out = self.lead(cells);
        out.push_str(value);
        let trimmed = out.trim_end_matches(' ').len();
        out.truncate(trimmed);
        out.push('\n');
        out
    }

    pub fn render(
        &self,
        cells: &[String],
        text: &str,
        total: usize,
        paint: impl Fn(&str) -> String,
    ) -> String {
        let start = self.text_column();
        let mut out = self.lead(cells);
        let room = total.saturating_sub(start).max(MIN_TEXT);
        let lines = wrap(text, room);
        let hanging = " ".repeat(start);
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                out.push('\n');
                out.push_str(&hanging);
            }
            out.push_str(&paint(line));
        }
        let trimmed = out.trim_end_matches(' ').len();
        out.truncate(trimmed);
        out.push('\n');
        out
    }
}

pub fn wrap_indented(
    text: &str,
    indent: usize,
    total: usize,
    paint: impl Fn(&str) -> String,
) -> String {
    let room = total.saturating_sub(indent).max(MIN_TEXT);
    wrap(text, room)
        .iter()
        .map(|line| format!("{}{}\n", " ".repeat(indent), paint(line)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_is_stripped_before_measuring() {
        let painted =
            "\u{1b}[1;36mcrepath\u{1b}[0m \u{1b}]8;;https://x\u{1b}\\link\u{1b}]8;;\u{1b}\\";
        assert_eq!(strip_ansi(painted), "crepath link");
        assert_eq!(width(painted), 12);
        assert_eq!(width("✔ ok"), 4);
        assert_eq!(width("日本"), 4);
        assert_eq!(pad("\u{1b}[31mab\u{1b}[0m", 4), "\u{1b}[31mab\u{1b}[0m  ");
        assert_eq!(pad_left("ab", 4), "  ab");
    }

    #[test]
    fn long_text_keeps_its_tail() {
        let text = "x".repeat(50) + "tail";
        let fitted = keep_tail(&text, 10, "…");
        assert_eq!(width(&fitted), 10);
        assert!(fitted.starts_with('…'));
        assert!(fitted.ends_with("tail"));
        assert_eq!(keep_tail("short", 10, "…"), "short");
    }

    #[test]
    fn wrapping_is_greedy_and_splits_long_words() {
        assert_eq!(wrap("one two three four", 9), ["one two", "three", "four"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn grid_aligns_and_hangs_wrapped_text() {
        let rows = [
            vec!["a".to_string(), "\u{1b}[1mlong\u{1b}[0m".to_string()],
            vec!["bbb".to_string(), "x".to_string()],
        ];
        let grid = Grid::fit(2, rows.iter().map(Vec::as_slice));
        assert_eq!(grid.text_column(), 2 + 3 + 2 + 4 + 2);
        let out = grid.render(&rows[1], "alpha beta gamma delta", 30, str::to_string);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "  bbb  x     alpha beta gamma");
        assert_eq!(lines[1], "             delta");
        for line in lines {
            assert!(width(line) <= 30);
        }
    }

    #[test]
    fn grids_can_share_a_text_column() {
        let mut grid = Grid::fit(2, [&["ab".to_string()][..]]);
        grid.align_text_to(10);
        assert_eq!(grid.text_column(), 10);
        grid.align_text_to(4);
        assert_eq!(grid.text_column(), 10);
        let mut wide = Grid::fit(2, [&["a".to_string(), "b".to_string()][..]]);
        assert_eq!(wide.column_start(1), 5);
        wide.align_column_to(1, 9);
        assert_eq!(wide.column_start(1), 9);
        assert_eq!(wide.text_column(), 12);
    }
}

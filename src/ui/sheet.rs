//! The one table shape every command renders: named columns, aligned cells, optional total row.

use comfy_table::{Attribute, Cell, CellAlignment, ColumnConstraint};
use std::fmt::{self, Display};

use super::{Theme, table};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

pub struct Sheet {
    theme: Theme,
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<Cell>>,
    total: Option<Vec<Cell>>,
    flex: Option<usize>,
}

impl Sheet {
    pub fn new(theme: Theme, columns: &[(&str, Align)]) -> Self {
        Self {
            theme,
            headers: columns.iter().map(|(name, _)| name.to_string()).collect(),
            aligns: columns.iter().map(|(_, align)| *align).collect(),
            rows: Vec::new(),
            total: None,
            flex: None,
        }
    }

    pub fn flex(mut self, index: usize) -> Self {
        self.flex = Some(index);
        self
    }

    pub fn row(&mut self, cells: Vec<Cell>) {
        assert_eq!(cells.len(), self.headers.len(), "{:?}", self.headers);
        self.rows.push(cells);
    }

    pub fn total(&mut self, cells: Vec<Cell>) {
        assert_eq!(cells.len(), self.headers.len(), "{:?}", self.headers);
        let theme = self.theme;
        self.total = Some(
            cells
                .into_iter()
                .map(|cell| {
                    if theme.color() {
                        cell.add_attribute(Attribute::Bold)
                    } else {
                        cell
                    }
                })
                .collect(),
        );
    }

    pub fn headers(&self) -> &[String] {
        &self.headers
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn column(&self, index: usize) -> Vec<String> {
        self.rows
            .iter()
            .chain(self.total.iter())
            .map(|row| row[index].content())
            .collect()
    }

    pub fn render(&self) -> String {
        let headers: Vec<&str> = self.headers.iter().map(String::as_str).collect();
        let mut table = table(self.theme, &headers);
        for row in self.rows.iter().chain(self.total.iter()) {
            table.add_row(row.clone());
        }
        for (index, column) in table.column_iter_mut().enumerate() {
            if Some(index) != self.flex {
                column.set_constraint(ColumnConstraint::ContentWidth);
            }
            column.set_cell_alignment(match self.aligns[index] {
                Align::Left => CellAlignment::Left,
                Align::Right => CellAlignment::Right,
            });
        }
        table.to_string()
    }
}

impl Display for Sheet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

pub const MIN_FLEX: usize = 16;

pub fn flex_room(total: usize, others: &[usize]) -> Option<usize> {
    let columns = others.len() + 1;
    let fixed = others.iter().map(|width| width + 2).sum::<usize>() + columns + 1;
    let room = total.checked_sub(fixed + 2)?;
    (room >= MIN_FLEX).then_some(room)
}

pub fn column_width<'a>(header: &str, cells: impl IntoIterator<Item = &'a str>) -> usize {
    cells
        .into_iter()
        .map(super::layout::width)
        .chain([super::layout::width(header)])
        .max()
        .unwrap_or(0)
}

pub fn is_bare_number(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty()
        && text
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, ',' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::layout::{strip_ansi, width};

    fn sample(theme: Theme) -> Sheet {
        let mut sheet = Sheet::new(
            theme,
            &[("kind", Align::Left), ("workspaces", Align::Right)],
        );
        sheet.row(vec![theme.cell("folder", None, &[]), Cell::new("52")]);
        sheet.row(vec![theme.cell("unsaved", None, &[]), Cell::new("4")]);
        sheet.total(vec![Cell::new("all kinds"), Cell::new("56")]);
        sheet
    }

    #[test]
    fn renders_a_header_and_aligned_rows() {
        let plain = sample(Theme::plain()).render();
        let lines: Vec<&str> = plain.lines().collect();
        assert!(lines[1].contains("kind") && lines[1].contains("workspaces"));
        assert!(lines.iter().all(|line| width(line) == width(lines[0])));
        assert!(lines[3].ends_with("52 │"));
        assert!(lines[4].ends_with(" 4 │"));
        assert!(plain.contains("all kinds"));
        assert_eq!(strip_ansi(&sample(Theme::colored()).render()), plain);
    }

    #[test]
    fn exposes_columns_for_checks() {
        let sheet = sample(Theme::plain());
        assert_eq!(sheet.headers(), ["kind", "workspaces"]);
        assert_eq!(sheet.column(1), ["52", "4", "56"]);
        assert_eq!(sheet.len(), 2);
        assert!(is_bare_number("1,092"));
        assert!(!is_bare_number("72.2%"));
        assert!(!is_bare_number("4 chats"));
    }

    #[test]
    fn flex_room_leaves_space_for_the_other_columns() {
        assert_eq!(flex_room(40, &[4, 6]), Some(40 - (6 + 8 + 4) - 2));
        assert_eq!(flex_room(20, &[4, 6]), None);
        assert_eq!(column_width("kind", ["● code-workspace", "x"]), 16);
        assert_eq!(column_width("workspaces", ["52"]), 10);
    }

    #[test]
    #[should_panic]
    fn rows_must_match_the_header() {
        let mut sheet = Sheet::new(Theme::plain(), &[("a", Align::Left)]);
        sheet.row(vec![Cell::new("1"), Cell::new("2")]);
    }
}

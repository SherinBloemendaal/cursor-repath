//! Block-character bars and percentage shares, rendered as table cells.

use comfy_table::{Attribute, Cell, CellAlignment, Color};

use super::Theme;

const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
pub const BAR_SPAN: usize = 24;

pub fn blocks(value: u64, max: u64, span: usize, unicode: bool) -> String {
    let max = max.max(1);
    let eighths = (u128::from(value) * span as u128 * 8 / u128::from(max)) as usize;
    if !unicode {
        let mut out = "#".repeat(eighths / 8);
        if out.is_empty() && value > 0 {
            out.push('#');
        }
        return out;
    }
    let mut out = "█".repeat(eighths / 8);
    out.push_str(PARTIAL[eighths % 8]);
    if out.is_empty() && value > 0 {
        out.push_str(PARTIAL[1]);
    }
    out
}

pub fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0.0%".to_string();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

impl Theme {
    pub fn bar_cell(self, value: u64, max: u64, color: Color) -> Cell {
        self.cell(
            blocks(value, max, BAR_SPAN, self.unicode()),
            Some(color),
            &[],
        )
    }

    pub fn share_cell(self, part: u64, whole: u64) -> Cell {
        self.cell(share(part, whole), None, &[Attribute::Dim])
            .set_alignment(CellAlignment::Right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_scale_to_the_largest_value() {
        assert_eq!(blocks(8, 8, 8, true), "████████");
        assert_eq!(blocks(4, 8, 8, true), "████");
        assert_eq!(blocks(0, 8, 8, true), "");
        assert_eq!(blocks(3, 8, 4, true), "█▌");
        assert_eq!(blocks(1, 1000, 10, true), "▏");
        assert_eq!(blocks(3, 8, 4, false), "#");
        assert_eq!(blocks(1, 1000, 10, false), "#");
    }

    #[test]
    fn shares_carry_a_percent_sign() {
        assert_eq!(share(1, 4), "25.0%");
        assert_eq!(share(0, 0), "0.0%");
        assert_eq!(share(52, 72), "72.2%");
    }
}

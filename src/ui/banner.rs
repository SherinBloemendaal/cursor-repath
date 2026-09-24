//! The CREPATH block-letter banner with a horizontal color gradient.

use owo_colors::{AnsiColors, Style, XtermColors};

use super::Theme;
use super::layout::{GAP, width, wrap_indented};
use super::term::Depth;

const INDENT: usize = 2;

const BLOCK: [[&str; 6]; 7] = [
    [
        " ██████╗",
        "██╔════╝",
        "██║     ",
        "██║     ",
        "╚██████╗",
        " ╚═════╝",
    ],
    [
        "██████╗ ",
        "██╔══██╗",
        "██████╔╝",
        "██╔══██╗",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ],
    [
        "███████╗",
        "██╔════╝",
        "█████╗  ",
        "██╔══╝  ",
        "███████╗",
        "╚══════╝",
    ],
    [
        "██████╗ ",
        "██╔══██╗",
        "██████╔╝",
        "██╔═══╝ ",
        "██║     ",
        "╚═╝     ",
    ],
    [
        " █████╗ ",
        "██╔══██╗",
        "███████║",
        "██╔══██║",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ],
    [
        "████████╗",
        "╚══██╔══╝",
        "   ██║   ",
        "   ██║   ",
        "   ██║   ",
        "   ╚═╝   ",
    ],
    [
        "██╗  ██╗",
        "██║  ██║",
        "███████║",
        "██╔══██║",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ],
];

const ASCII: [[&str; 5]; 7] = [
    ["  ____ ", " / ___|", "| |    ", "| |___ ", " \\____|"],
    [" ____  ", "|  _ \\ ", "| |_) |", "|  _ < ", "|_| \\_\\"],
    [" _____ ", "| ____|", "|  _|  ", "| |___ ", "|_____|"],
    [" ____  ", "|  _ \\ ", "| |_) |", "|  __/ ", "|_|    "],
    [
        "    _    ",
        "   / \\   ",
        "  / _ \\  ",
        " / ___ \\ ",
        "/_/   \\_\\",
    ],
    [" _____ ", "|_   _|", "  | |  ", "  | |  ", "  |_|  "],
    [" _   _ ", "| | | |", "| |_| |", "|  _  |", "|_| |_|"],
];

const STOPS: [(u8, u8, u8); 3] = [(0, 212, 255), (124, 92, 255), (255, 80, 200)];

pub fn art(unicode: bool) -> Vec<String> {
    if unicode {
        compose(&BLOCK, "")
    } else {
        compose(&ASCII, " ")
    }
}

fn compose<const ROWS: usize>(font: &[[&str; ROWS]], spacing: &str) -> Vec<String> {
    (0..ROWS)
        .map(|row| {
            font.iter()
                .map(|glyph| glyph[row])
                .collect::<Vec<_>>()
                .join(spacing)
        })
        .collect()
}

pub fn banner(theme: Theme, total: usize) -> String {
    let lines = art(theme.unicode());
    let art_width = lines.iter().map(|line| width(line)).max().unwrap_or(0);
    let mut out = String::new();
    if INDENT + art_width <= total {
        for line in &lines {
            out.push_str(&" ".repeat(INDENT));
            out.push_str(&paint_line(theme, line, art_width));
            out.push('\n');
        }
        out.push('\n');
    }
    let tagline = "Repath Cursor workspaces and chats.";
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    if INDENT + width(tagline) + GAP + width(&version) <= total {
        out.push_str(&format!(
            "{}{}{}{}\n",
            " ".repeat(INDENT),
            theme.bold(tagline),
            " ".repeat(GAP),
            theme.dim(&version)
        ));
    } else {
        out.push_str(&wrap_indented(tagline, INDENT, total, |line| {
            theme.bold(line)
        }));
        out.push_str(&wrap_indented(&version, INDENT, total, |line| {
            theme.dim(line)
        }));
    }
    out
}

fn paint_line(theme: Theme, line: &str, span: usize) -> String {
    if !theme.color() {
        return line.trim_end().to_string();
    }
    let mut out = String::new();
    for (column, ch) in line.trim_end().chars().enumerate() {
        if ch == ' ' {
            out.push(ch);
            continue;
        }
        let fraction = column as f32 / span.saturating_sub(1).max(1) as f32;
        let shadow = !matches!(ch, '█') && theme.unicode();
        out.push_str(&theme.paint(ch, gradient_style(theme.depth(), fraction, shadow)));
    }
    out
}

pub fn gradient_style(depth: Depth, fraction: f32, shadow: bool) -> Style {
    let (r, g, b) = blend(fraction);
    let (r, g, b) = if shadow {
        (shade(r), shade(g), shade(b))
    } else {
        (r, g, b)
    };
    let style = match depth {
        Depth::TrueColor => Style::new().truecolor(r, g, b),
        Depth::Ansi256 => Style::new().color(XtermColors::from(xterm(r, g, b))),
        Depth::Basic => {
            let color = if fraction < 0.34 {
                AnsiColors::Cyan
            } else if fraction < 0.67 {
                AnsiColors::Blue
            } else {
                AnsiColors::Magenta
            };
            let style = Style::new().color(color);
            return if shadow { style.dimmed() } else { style.bold() };
        }
    };
    if shadow { style } else { style.bold() }
}

fn blend(fraction: f32) -> (u8, u8, u8) {
    let fraction = fraction.clamp(0.0, 1.0);
    let scaled = fraction * (STOPS.len() - 1) as f32;
    let index = (scaled.floor() as usize).min(STOPS.len() - 2);
    let local = scaled - index as f32;
    let (from, to) = (STOPS[index], STOPS[index + 1]);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * local).round() as u8;
    (mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

fn shade(channel: u8) -> u8 {
    (channel as f32 * 0.55).round() as u8
}

fn xterm(r: u8, g: u8, b: u8) -> u8 {
    let level = |value: u8| match value {
        0..48 => 0,
        48..115 => 1,
        _ => (value - 35) / 40,
    };
    16 + 36 * level(r) + 6 * level(g) + level(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::layout::strip_ansi;

    #[test]
    fn every_glyph_row_has_the_same_width() {
        for glyph in BLOCK {
            let widths: Vec<usize> = glyph.iter().map(|row| width(row)).collect();
            assert!(widths.iter().all(|w| *w == widths[0]), "{glyph:?}");
        }
        for glyph in ASCII {
            let widths: Vec<usize> = glyph.iter().map(|row| width(row)).collect();
            assert!(widths.iter().all(|w| *w == widths[0]), "{glyph:?}");
        }
        for unicode in [true, false] {
            let lines = art(unicode);
            assert!(lines.iter().all(|line| width(line) == width(&lines[0])));
        }
    }

    #[test]
    fn plain_banner_has_no_escape_codes() {
        let out = banner(Theme::plain(), 100);
        assert!(!out.contains('\u{1b}'));
        assert!(out.contains("██████╗"));
        assert!(out.contains(concat!("v", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn gradient_uses_the_detected_depth() {
        let truecolor = banner(Theme::colored().with_depth(Depth::TrueColor), 100);
        assert!(truecolor.contains("\u{1b}[38;2;"));
        let xterm = banner(Theme::colored().with_depth(Depth::Ansi256), 100);
        assert!(xterm.contains("\u{1b}[38;5;"));
        assert_eq!(strip_ansi(&truecolor), banner(Theme::plain(), 100));
    }

    #[test]
    fn narrow_terminals_skip_the_art() {
        let out = banner(Theme::plain(), 40);
        assert!(!out.contains('█'));
        assert!(out.contains("Repath Cursor workspaces and chats."));
        assert!(out.lines().all(|line| width(line) <= 40));
    }

    #[test]
    fn cube_mapping() {
        assert_eq!(xterm(0, 0, 0), 16);
        assert_eq!(xterm(255, 255, 255), 231);
        assert_eq!(blend(0.0), STOPS[0]);
        assert_eq!(blend(1.0), STOPS[2]);
    }
}

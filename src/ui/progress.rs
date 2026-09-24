//! Dual progress bars (step and operation) and a single-line spinner, drawn on stderr.

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::io::{self, IsTerminal};
use std::time::Duration;

use super::Theme;

const UNICODE_TICKS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";
const ASCII_TICKS: &str = "|/-\\ ";
const UNICODE_BAR: &str = "━╸─";
const ASCII_BAR: &str = "=> ";

fn style(theme: Theme, template: &str) -> ProgressStyle {
    let (ticks, bar) = if theme.unicode() {
        (UNICODE_TICKS, UNICODE_BAR)
    } else {
        (ASCII_TICKS, ASCII_BAR)
    };
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .tick_chars(ticks)
        .progress_chars(bar)
}

fn dual_styles(theme: Theme) -> (ProgressStyle, ProgressStyle) {
    if theme.color() {
        (
            style(
                theme,
                "{spinner:.cyan.bold} {prefix:<6.bold.blue} {bar:32.green/black.bright} {pos:>4}/{len:<4} {elapsed:>4.dim} {wide_msg}",
            ),
            style(
                theme,
                "  {spinner:.magenta} {prefix:<4.dim} {bar:32.cyan/black.bright} {pos:>4}/{len:<4} eta {eta:.yellow} {wide_msg:.dim}",
            ),
        )
    } else {
        (
            style(
                theme,
                "{spinner} {prefix:<6} [{bar:32}] {pos:>4}/{len:<4} {elapsed:>4} {wide_msg}",
            ),
            style(
                theme,
                "  {spinner} {prefix:<4} [{bar:32}] {pos:>4}/{len:<4} eta {eta} {wide_msg}",
            ),
        )
    }
}

pub struct DualProgress {
    theme: Theme,
    steps: ProgressBar,
    rows: ProgressBar,
    _multi: Option<MultiProgress>,
}

impl DualProgress {
    pub fn new(label: &str, steps: u64, quiet: bool) -> Self {
        let theme = Theme::stderr();
        if quiet || !io::stderr().is_terminal() {
            let steps =
                ProgressBar::with_draw_target(Some(steps.max(1)), ProgressDrawTarget::hidden());
            let rows = ProgressBar::with_draw_target(Some(1), ProgressDrawTarget::hidden());
            return Self {
                theme,
                steps,
                rows,
                _multi: None,
            };
        }
        let (style_steps, style_rows) = dual_styles(theme);
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
        let steps_bar = multi.add(ProgressBar::new(steps.max(1)));
        let rows_bar = multi.add(ProgressBar::new(1));
        steps_bar.set_style(style_steps);
        steps_bar.set_prefix(label.to_string());
        rows_bar.set_style(style_rows);
        rows_bar.set_prefix("rows");
        steps_bar.enable_steady_tick(Duration::from_millis(100));
        rows_bar.enable_steady_tick(Duration::from_millis(100));
        Self {
            theme,
            steps: steps_bar,
            rows: rows_bar,
            _multi: Some(multi),
        }
    }

    pub fn step(&self, index: u64, message: &str) {
        self.steps.set_position(index);
        let styled = message
            .split(' ')
            .map(|word| self.theme.token(word))
            .collect::<Vec<_>>()
            .join(" ");
        self.steps.set_message(styled);
    }

    pub fn rows(&self, done: u64, total: u64, message: &str) {
        self.rows.set_length(total.max(1));
        self.rows.set_position(done.min(total.max(1)));
        self.rows.set_message(message.to_string());
    }

    pub fn finish(&self) {
        self.steps.finish_and_clear();
        self.rows.finish_and_clear();
    }
}

pub struct Spinner(Option<ProgressBar>);

impl Spinner {
    pub fn set_message(&self, message: &str) {
        if let Some(bar) = &self.0 {
            bar.set_message(message.to_string());
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some(bar) = self.0.take() {
            bar.finish_and_clear();
        }
    }
}

pub fn spinner(message: &str, quiet: bool) -> Spinner {
    if quiet || !io::stderr().is_terminal() {
        return Spinner(None);
    }
    let theme = Theme::stderr();
    let template = if theme.color() {
        "{spinner:.cyan.bold} {msg} {elapsed:.dim}"
    } else {
        "{spinner} {msg} {elapsed}"
    };
    let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr());
    bar.set_style(style(theme, template));
    bar.set_message(message.to_string());
    bar.enable_steady_tick(Duration::from_millis(80));
    Spinner(Some(bar))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_parse_in_every_theme() {
        for theme in [Theme::plain(), Theme::colored(), Theme::ascii()] {
            let (steps, rows) = dual_styles(theme);
            let bar = ProgressBar::hidden();
            bar.set_style(steps);
            bar.set_style(rows);
        }
        let hidden = DualProgress::new("move", 3, true);
        hidden.step(1, "abc12345 /tmp/app");
        hidden.rows(5, 10, "rows");
        hidden.finish();
    }
}

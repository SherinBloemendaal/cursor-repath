//! Interactive prompts on stderr, themed to match the status lines.

use owo_colors::Style;
use std::fmt;
use std::io::{self, IsTerminal};

use super::{Theme, hinted};

struct ConfirmTheme(Theme);

impl dialoguer::theme::Theme for ConfirmTheme {
    fn format_confirm_prompt(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        default: Option<bool>,
    ) -> fmt::Result {
        let theme = self.0;
        let choices = match default {
            Some(true) => format!("[{}/n]", theme.bold("Y")),
            Some(false) => format!("[y/{}]", theme.bold("N")),
            None => "[y/n]".to_string(),
        };
        write!(
            f,
            "{} {} {} ",
            theme.paint("?", Style::new().bold().yellow()),
            theme.bold(prompt),
            theme.dim(choices)
        )
    }

    fn format_confirm_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        selection: Option<bool>,
    ) -> fmt::Result {
        let theme = self.0;
        let icons = theme.icons();
        match selection {
            Some(true) => write!(
                f,
                "{} {}{}{}",
                theme.good(icons.check),
                theme.bold(prompt),
                theme.sep(),
                theme.good("yes")
            ),
            Some(false) => write!(
                f,
                "{} {}{}{}",
                theme.bad(icons.cross),
                theme.bold(prompt),
                theme.sep(),
                theme.bad("no")
            ),
            None => write!(f, "{} {}", theme.dim("?"), theme.bold(prompt)),
        }
    }
}

fn with_prompt_theme<R>(run: impl FnOnce(&dyn dialoguer::theme::Theme) -> R) -> R {
    let theme = Theme::stderr();
    if theme.color() && theme.unicode() {
        run(&dialoguer::theme::ColorfulTheme::default())
    } else {
        run(&dialoguer::theme::SimpleTheme)
    }
}

pub fn confirm(prompt: &str, yes: bool) -> anyhow::Result<bool> {
    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        return Err(hinted(prompt, "Re-run with -y to continue."));
    }
    let theme = ConfirmTheme(Theme::stderr());
    let choice = dialoguer::Confirm::with_theme(&theme)
        .with_prompt(prompt)
        .default(false)
        .interact()?;
    Ok(choice)
}

pub fn multi_select(prompt: &str, items: &[String]) -> anyhow::Result<Vec<usize>> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("pass arguments, or run {prompt} in a terminal");
    }
    let picked = with_prompt_theme(|theme| {
        dialoguer::MultiSelect::with_theme(theme)
            .with_prompt(prompt)
            .items(items)
            .interact()
    })?;
    Ok(picked)
}

pub fn input(prompt: &str) -> anyhow::Result<String> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("pass arguments, or run in a terminal");
    }
    Ok(with_prompt_theme(|theme| {
        dialoguer::Input::with_theme(theme)
            .with_prompt(prompt)
            .interact_text()
    })?)
}

pub fn select(prompt: &str, items: &[&str]) -> anyhow::Result<usize> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("pass arguments, or run in a terminal");
    }
    Ok(with_prompt_theme(|theme| {
        dialoguer::Select::with_theme(theme)
            .with_prompt(prompt)
            .items(items)
            .default(0)
            .interact()
    })?)
}

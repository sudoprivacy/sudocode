//! Web-link validation and terminal presentation, shared by every text adapter.

use std::{fmt, io::IsTerminal, sync::Arc};

/// Bound both terminal metadata and copies retained by a live canvas.
pub(super) const MAX_DESTINATION_BYTES: usize = 8 * 1024;

pub(crate) fn destination(value: &str) -> Option<Arc<str>> {
    if value.len() > MAX_DESTINATION_BYTES || value.chars().any(char::is_control) {
        return None;
    }
    let parsed = reqwest::Url::parse(value).ok()?;
    (matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some())
        .then(|| Arc::from(value))
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LinkDisplay {
    Plain,
    WithDestination,
    Label,
}

impl LinkDisplay {
    pub(crate) fn detect() -> Self {
        static DISPLAY: std::sync::OnceLock<LinkDisplay> = std::sync::OnceLock::new();
        *DISPLAY.get_or_init(Self::detect_terminal)
    }

    fn detect_terminal() -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        if !std::io::stdout().is_terminal() || term == "dumb" {
            return Self::Plain;
        }
        if ["TMUX", "STY"]
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
            || term.starts_with("screen")
            || term.starts_with("tmux")
        {
            return Self::WithDestination;
        }
        let program = std::env::var("TERM_PROGRAM")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            program.as_str(),
            "iterm.app" | "wezterm" | "ghostty" | "vscode" | "kitty" | "alacritty" | "rex"
        ) || ["WT_SESSION", "VTE_VERSION", "KONSOLE_VERSION"]
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
            || term == "xterm-kitty"
        {
            Self::Label
        } else {
            Self::WithDestination
        }
    }

    pub(crate) fn enabled(self) -> bool {
        !matches!(self, Self::Plain)
    }

    pub(crate) fn show_destination(self) -> bool {
        !matches!(self, Self::Label)
    }
}

/// Only validated metadata may introduce OSC bytes at the output boundary.
pub(crate) struct SetHyperlink<'a>(pub(crate) Option<&'a str>);

impl crossterm::Command for SetHyperlink<'_> {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b]8;;{}\x1b\\", self.0.unwrap_or_default())
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

impl fmt::Display for SetHyperlink<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crossterm::Command::write_ansi(self, f)
    }
}

//! Startup scrollback uses the renderer's terminal-cell and ANSI layout.

use super::text_layout::{display_width, wrap_ansi_to_width, wrap_ansi_with_indent};
use super::{theme, TextRole, RESET};

pub(crate) fn banner(lines: &[String]) -> String {
    let terminal_width = super::query_terminal_width().max(1);
    let t = theme();
    let logo_fg = t.fg_seq(TextRole::Brand);
    let accent_fg = t.fg_seq(TextRole::Accent);
    let logo = format!(
        "{logo_fg}\
███████╗██╗   ██╗██████╗  ██████╗ \n\
██╔════╝██║   ██║██╔══██╗██╔═══██╗\n\
███████╗██║   ██║██║  ██║██║   ██║\n\
╚════██║██║   ██║██║  ██║██║   ██║\n\
███████║╚██████╔╝██████╔╝╚██████╔╝\n\
╚══════╝ ╚═════╝ ╚═════╝  ╚═════╝{RESET} {accent_fg}Code{RESET}"
    );
    let logo = if logo
        .lines()
        .all(|line| display_width(line) <= terminal_width)
    {
        logo
    } else {
        wrap_ansi_to_width(
            &format!("{logo_fg}Sudo{RESET}{accent_fg}Code{RESET}"),
            terminal_width,
        )
        .join("\n")
    };

    // The border and its padding consume four columns. All data fields
    // share the same ANSI/grapheme layout as the rest of scrollback.
    let content_width = terminal_width.saturating_sub(4);
    let is_boxed = content_width >= 2;
    let width = if is_boxed {
        content_width
    } else {
        terminal_width
    };
    let lines: Vec<_> = lines
        .iter()
        .flat_map(|line| line.lines())
        .flat_map(|line| wrap_ansi_with_indent(line, width, 2))
        .collect();
    if !is_boxed {
        return format!("{logo}\n\n{}", lines.join("\n"));
    }
    let max_width = lines
        .iter()
        .map(|line| display_width(line))
        .max()
        .unwrap_or(0);
    let box_width = max_width + 2; // 1 space padding on each side

    let grey = t.fg_seq(TextRole::Border);
    let reset = RESET;

    let top = format!("{grey}╭{}╮{reset}", "─".repeat(box_width));
    let bottom = format!("{grey}╰{}╯{reset}", "─".repeat(box_width));

    let boxed_lines: Vec<String> = lines
        .iter()
        .map(|line| {
            let visible_width = display_width(line);
            let padding = max_width - visible_width;
            format!(
                "{grey}│{reset} {}{} {grey}│{reset}",
                line,
                " ".repeat(padding)
            )
        })
        .collect();

    format!(
        "{}\n\n{}\n{}\n{}",
        logo,
        top,
        boxed_lines.join("\n"),
        bottom,
    )
}

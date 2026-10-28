//! One bounded footer: permission, background work, and one contextual tip.
use super::tasks::{TaskBrowser, TaskFocus};
use crate::render::{
    styled_text::StyledText,
    text_layout::{display_width, truncate_to_width},
    theme, ColorSupport,
};
use crossterm::style::ContentStyle;
use engine_core::{Tip, TipPriority};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Hash)]
pub(super) struct FooterHints<'a> {
    pub transient: Option<&'a str>,
    /// What the keys do while typing; `None` when the input is empty.
    pub controls: Option<&'a str>,
    /// The one key that matters while not typing, after the workspace position.
    pub ambient: &'a str,
}

#[derive(Clone, Default, Hash)]
pub(super) struct FooterTips {
    model: BTreeMap<TipPriority, Tip>,
    cache: Option<Tip>,
    /// Where the workspace is, `repo ⎇ branch`. State rather than advice, so it
    /// is not a `Tip`: it leads the not-typing filler and yields to every tip.
    position: Option<String>,
}

impl FooterTips {
    pub fn set(&mut self, tip: Tip) {
        if tip.validate().is_err() {
            return;
        }
        if tip.message.trim().is_empty() {
            self.model.remove(&tip.priority);
        } else {
            self.model.insert(tip.priority, tip);
        }
    }
    pub fn cache(&mut self, tip: Option<Tip>) {
        self.cache = tip;
    }
    pub fn position(&mut self, label: Option<String>) {
        self.position = label;
    }
    pub fn begin_turn(&mut self) {
        self.model.clear();
    }

    pub fn render(
        &self,
        permission: &str,
        tasks: &TaskBrowser,
        active: bool,
        hints: FooterHints<'_>,
        width: usize,
    ) -> StyledText {
        let native = hints.transient.map(|message| Tip {
            priority: TipPriority::Urgent,
            message: StyledText::from_ansi(message)
                .text
                .replace(['\n', '\r', '\t'], " "),
        });
        let control = hints.controls.map(|message| Tip {
            priority: TipPriority::Normal,
            message: message.to_string(),
        });
        // Not typing: where the workspace is, then the key that matters now.
        // The position leads so truncation trims the key hint first.
        let ambient = Tip {
            priority: TipPriority::Normal,
            message: match &self.position {
                Some(position) => format!("{position} · {}", hints.ambient),
                None => hints.ambient.into(),
            },
        };
        // A focused cache tip is the footer's selection and says how to act on it.
        let tip_focused = tasks.focus == TaskFocus::Tip;
        let focused = self.cache.as_ref().filter(|_| tip_focused).map(|tip| Tip {
            priority: TipPriority::Important,
            message: format!("{} · Enter to review", tip.message),
        });
        let selected = native
            .as_ref()
            .or_else(|| {
                let model = self.model.values().next_back();
                match (model, self.cache.as_ref()) {
                    (Some(m), Some(c)) if c.priority > m.priority => Some(c),
                    (Some(m), _) => Some(m),
                    (_, cache) => cache,
                }
            })
            .filter(|tip| !active || tip.priority == TipPriority::Urgent)
            .or(control.as_ref())
            .or(Some(&ambient))
            .filter(|tip| tasks.focus == TaskFocus::Input || tip.priority == TipPriority::Urgent);
        let selected = focused.as_ref().or(selected);

        let mut base = truncate_to_width(&format!("  ⏵⏵ {permission}"), width);
        let mut task = tasks.footer_label(false);
        if tasks.focus == TaskFocus::Footer
            && display_width(&base) + display_width(&task) + 3 > width
        {
            task = tasks.footer_label(true);
            base = truncate_to_width(&base, width.saturating_sub(display_width(&task) + 3));
        }
        let high = selected.is_some_and(|tip| tip.priority >= TipPriority::Important);
        let sep = " · ";
        let required = |base: &str, task: &str| {
            display_width(base)
                + if task.is_empty() {
                    0
                } else {
                    display_width(sep) + display_width(task)
                }
        };
        if high
            && required(&base, &task)
                + display_width(sep)
                + display_width(&selected.unwrap().message)
                > width
        {
            task = tasks.footer_label(true);
            if width.saturating_sub(required(&base, &task) + display_width(sep)) < 16 {
                task.clear();
            }
            if width.saturating_sub(required(&base, &task) + display_width(sep)) < 16 {
                base = truncate_to_width(&base, width.saturating_sub(display_width(sep) + 16));
            }
        }
        let available_task = width.saturating_sub(display_width(&base) + display_width(sep));
        task = truncate_to_width(&task, available_task);
        let remaining = width.saturating_sub(required(&base, &task) + display_width(sep));
        let tip = selected
            .filter(|_| remaining >= 8)
            .map(|tip| (tip, truncate_to_width(&tip.message, remaining)));
        let support = ColorSupport::detect();
        let colors = support != ColorSupport::NoColor;
        let palette = theme();
        let style = |color| ContentStyle {
            foreground_color: colors.then_some(color),
            ..ContentStyle::default()
        };
        let muted = style(palette.muted);
        let mut result = StyledText::default();
        result.push(muted, &base);
        if !task.is_empty() {
            result.push(muted, sep);
            let task_style = if tasks.focus == TaskFocus::Footer {
                palette.selection_style(support)
            } else {
                muted
            };
            result.push(task_style, &task);
        }
        if let Some((tip, text)) = tip {
            result.push(muted, sep);
            let color = match tip.priority {
                TipPriority::Urgent => palette.warning,
                TipPriority::Important => palette.primary,
                TipPriority::Normal => palette.muted,
            };
            if tip_focused {
                result.push(palette.selection_style(support), &text);
            } else {
                result.push(style(color), &text);
            }
        }
        result
    }
}

//! One bounded footer: permission, background work, and one contextual tip.
use super::tasks::{TaskBrowser, TaskFocus};
use crate::render::TextRole;
use crate::render::{
    styled_text::StyledText,
    text_layout::{display_width, truncate_to_width},
    theme, ColorSupport,
};
use engine_core::{Tip, TipPriority};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Hash)]
pub(super) struct FooterHints<'a> {
    pub transient: Option<&'a str>,
    /// The keys for the current turn and queue state, after the position.
    pub keys: &'a str,
}

/// Who owns a persistent footer tip. Unlike the model's tips these outlive a
/// turn; each source replaces or clears only its own. A later source wins a
/// priority tie.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum StatusSource {
    /// Prompt-cache advice; the only tip the footer can focus and act on.
    Cache,
    /// Peer-messaging health: shown only while degraded, gone once it recovers.
    A2a,
}

#[derive(Clone, Default, Hash)]
pub(super) struct FooterTips {
    model: BTreeMap<TipPriority, Tip>,
    status: BTreeMap<StatusSource, Tip>,
    /// Where the workspace is, `repo ⎇ branch`. State rather than advice, so it
    /// is not a `Tip`: it leads the filler and yields to every tip.
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
    pub fn status(&mut self, source: StatusSource, tip: Option<Tip>) {
        match tip {
            Some(tip) => self.status.insert(source, tip),
            None => self.status.remove(&source),
        };
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
        // Where the workspace is, then the keys for the current state. The
        // position leads so truncation trims the keys first.
        let filler = Tip {
            priority: TipPriority::Normal,
            message: match &self.position {
                Some(position) => format!("{position} · {}", hints.keys),
                None => hints.keys.into(),
            },
        };
        // A focused cache tip is the footer's selection and says how to act on it.
        let tip_focused = tasks.focus == TaskFocus::Tip;
        let cache = self.status.get(&StatusSource::Cache);
        let focused = cache.filter(|_| tip_focused).map(|tip| Tip {
            priority: TipPriority::Important,
            message: format!("{} · Enter to review", tip.message),
        });
        let selected = native
            .as_ref()
            .or_else(|| {
                let model = self.model.values().next_back();
                let status = self
                    .status
                    .iter()
                    .max_by_key(|(source, tip)| (tip.priority, **source))
                    .map(|(_, tip)| tip);
                match (model, status) {
                    (Some(m), Some(s)) if s.priority > m.priority => Some(s),
                    (Some(m), _) => Some(m),
                    (_, status) => status,
                }
            })
            .filter(|tip| !active || tip.priority == TipPriority::Urgent)
            .or(Some(&filler))
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
        let palette = theme();
        let muted = palette.style_for(TextRole::Secondary, support);
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
            let role = match tip.priority {
                TipPriority::Urgent => TextRole::Warning,
                TipPriority::Important => TextRole::Notice,
                TipPriority::Normal => TextRole::Secondary,
            };
            if tip_focused {
                result.push(palette.selection_style(support), &text);
            } else if std::ptr::eq(tip, &raw const filler) {
                self.append_position(&mut result, &text);
            } else {
                result.push(palette.style(role), &text);
            }
        }

        result
    }
    /// Style only the locally owned workspace prefix; advice is never parsed.
    fn append_position(&self, result: &mut StyledText, text: &str) {
        let palette = theme();
        let Some(position) = &self.position else {
            result.push(palette.style(TextRole::Secondary), text);
            return;
        };
        let mut end = position.len().min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let prefix = &text[..end];
        if let Some((path, branch)) = prefix.split_once(" ⎇ ") {
            result.push(palette.style(TextRole::Path), path);
            result.push(palette.style(TextRole::Secondary), " ⎇ ");
            result.push(palette.style(TextRole::Branch), branch);
        } else {
            result.push(palette.style(TextRole::Path), prefix);
        }
        result.push(palette.style(TextRole::Secondary), &text[end..]);
    }
}

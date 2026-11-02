//! Task browser with bounded, lazily decoded output and event-driven repaint.
use super::choice_text::ChoiceText;
use crate::render::{
    ansi_fg,
    styled_text::StyledText,
    text_layout::{display_width, truncate_to_width, wrap_line},
    theme, RESET,
};
use engine_core::cache_advisor::{ttl_label, TtlAdvice, MIN_INTERVALS};
use engine_core::cache_ttl::TtlChoice;
use engine_events::{
    BackgroundTask, BackgroundTaskEvent, BackgroundTaskKind, BackgroundTaskStatus,
};
use std::{collections::BTreeMap, fmt::Write as _, time::Instant};

/// One projection of task events for the status row and footer. Reading state
/// belongs to this browser; execution state always comes from the producer.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(super) struct TaskSummary {
    terminals: usize,
    agents: usize,
    running: usize,
    stopping: usize,
    unconfirmed: usize,
    unread: usize,
    failed: usize,
}

impl TaskSummary {
    /// Attention states keep priority over the remaining running work.
    pub fn animates_status(self) -> bool {
        self.unconfirmed == 0 && self.failed == 0 && self.running + self.stopping > 0
    }

    pub fn status_lines(
        self,
        width: usize,
        completed_turn: Option<u32>,
        frame: usize,
    ) -> Option<(std::sync::Arc<StyledText>, std::sync::Arc<StyledText>)> {
        let colors = theme().activity_colors(false);
        let active = self.running + self.stopping;
        let (full, short, color) = if self.unconfirmed > 0 {
            (
                format!(
                    "{} unconfirmed",
                    quantity(self.unconfirmed, "background task")
                ),
                format!("{} unconfirmed", self.unconfirmed),
                theme().activity_colors(true).label,
            )
        } else if self.failed > 0 {
            (
                format!("{} failed", quantity(self.failed, "background task")),
                format!("{} failed", self.failed),
                theme().error,
            )
        } else if self.stopping > 0 {
            (
                format!("Stopping {}…", quantity(self.stopping, "background task")),
                format!("{} stopping", self.stopping),
                colors.logo,
            )
        } else if self.running > 0 {
            (
                format!("{} still running", quantity(self.running, "task")),
                format!("{} running", self.running),
                colors.logo,
            )
        } else {
            return None;
        };
        let suffix = if (self.unconfirmed > 0 || self.failed > 0) && active > 0 {
            format!(" · {active} active")
        } else if self.stopping > 0 && self.running > 0 {
            format!(" · {} still running", self.running)
        } else {
            String::new()
        };
        let full = format!("{full}{suffix}");
        let metadata = completed_turn
            .map(|turn| format!(" · turn {turn} finished"))
            .unwrap_or_default();
        let logo = self
            .animates_status()
            .then(|| crate::render::activity::spinner_frame(frame));
        let logo_width = logo.map_or(0, |logo| display_width(logo) + 1);
        let has_color =
            crate::render::ColorSupport::detect() != crate::render::ColorSupport::NoColor;
        let style = |color| crossterm::style::ContentStyle {
            foreground_color: has_color.then_some(color),
            ..Default::default()
        };
        let render = |label: &str, metadata: &str| {
            let mut text = StyledText::default();
            if let Some(logo) = logo {
                text.push(
                    style(colors.logo),
                    &truncate_to_width(&format!("{logo} "), width),
                );
            }
            text.push(
                style(color),
                &truncate_to_width(label, width.saturating_sub(logo_width)),
            );
            text.push(style(colors.metadata), metadata);
            std::sync::Arc::new(text)
        };
        let summary = render(&short, "");
        let full = if logo_width + display_width(&full) + display_width(&metadata) > width {
            summary.clone()
        } else {
            render(&full, &metadata)
        };
        Some((full, summary))
    }
}

fn quantity(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) enum TaskFocus {
    #[default]
    Input,
    /// The footer's background-task label.
    Footer,
    List,
    Detail,
    /// The footer's actionable cache-TTL tip.
    Tip,
    /// The cache-TTL panel opened from that tip.
    Ttl,
}

/// Rows of the cache-TTL panel, top to bottom.
const TTL_CHOICES: [TtlChoice; 3] = [TtlChoice::OneHour, TtlChoice::FiveMinutes, TtlChoice::Auto];
const COMMAND_PREVIEW_ROWS: usize = 3;

struct Entry {
    task: BackgroundTask,
    output: Option<StyledText>,
    read: bool,
    received: Instant,
    wrapped_width: usize,
    wrapped: Vec<StyledText>,
    command_rows: Option<(usize, Vec<String>)>,
}

#[derive(Default)]
pub(super) struct TaskBrowser {
    entries: BTreeMap<String, Entry>,
    rows: Vec<String>,
    summary: TaskSummary,
    footer_summary: String,
    compact_summary: String,
    pub focus: TaskFocus,
    selected: Option<String>,
    scroll: usize,
    /// The advice behind the cache-TTL tip, and whether the tip offers a switch.
    ttl_advice: Option<TtlAdvice>,
    tip_actionable: bool,
    ttl_row: usize,
}

impl TaskBrowser {
    /// Apply producer state and report whether the visible browser changed.
    pub fn update(&mut self, event: BackgroundTaskEvent) -> bool {
        let previous_summary = self.summary;
        let previous_focus = self.focus;
        match event {
            BackgroundTaskEvent::Updated(task) => {
                let read = self
                    .entries
                    .get(&task.id)
                    .is_some_and(|e| e.read && !e.task.status.is_active())
                    || (self.focus == TaskFocus::Detail
                        && self.selected.as_deref() == Some(&task.id));
                if let Some(entry) = self.entries.get_mut(&task.id) {
                    if entry.command_rows.is_some() && entry.task.command != task.command {
                        entry.command_rows = None;
                    }
                    if entry.task.output != task.output {
                        entry.output = None;
                        entry.wrapped_width = 0;
                        entry.wrapped.clear();
                    }
                    entry.task = task;
                    entry.read = read;
                    entry.received = Instant::now();
                } else {
                    self.entries.insert(
                        task.id.clone(),
                        Entry {
                            output: None,
                            task,
                            read,
                            received: Instant::now(),
                            wrapped_width: 0,
                            wrapped: Vec::new(),
                            command_rows: None,
                        },
                    );
                }
            }
            BackgroundTaskEvent::Removed(id) => {
                self.entries.remove(&id);
            }
        }
        self.rows = self
            .entries
            .values()
            .filter(|e| e.task.background)
            .map(|e| e.task.id.clone())
            .collect();
        self.rows.sort_by_key(|id| {
            let task = &self.entries[id].task;
            (
                !task.status.is_active(),
                std::cmp::Reverse(task.started_ms),
                id.clone(),
            )
        });
        self.refresh_summary();
        self.clamp();
        self.is_panel() || self.summary != previous_summary || self.focus != previous_focus
    }

    fn refresh_summary(&mut self) {
        let mut summary = TaskSummary::default();
        for id in &self.rows {
            let entry = &self.entries[id];
            if entry.task.status.is_active() {
                match entry.task.kind {
                    BackgroundTaskKind::Terminal => summary.terminals += 1,
                    BackgroundTaskKind::Agent => summary.agents += 1,
                }
                if entry.task.status == BackgroundTaskStatus::Stopping {
                    summary.stopping += 1;
                } else {
                    summary.running += 1;
                }
            } else if entry.task.status == BackgroundTaskStatus::ExecutionUnconfirmed {
                summary.unconfirmed += 1;
            } else if !entry.read {
                summary.unread += 1;
                summary.failed += usize::from(entry.task.status == BackgroundTaskStatus::Failed);
            }
        }
        if self.summary == summary {
            return;
        }
        self.summary = summary;
        let mut parts = Vec::new();
        if summary.terminals > 0 {
            parts.push(quantity(summary.terminals, "terminal"));
        }
        if summary.agents > 0 {
            parts.push(quantity(summary.agents, "agent"));
        }
        if summary.unconfirmed > 0 {
            parts.push(format!("{} unconfirmed", summary.unconfirmed));
        }
        if summary.unread > 0 {
            parts.push(quantity(summary.unread, "new result"));
        }
        self.footer_summary = parts.join(" · ");
        let mut compact = Vec::new();
        if summary.terminals > 0 {
            compact.push(format!("{}t", summary.terminals));
        }
        if summary.agents > 0 {
            compact.push(format!("{}a", summary.agents));
        }
        if summary.unconfirmed > 0 {
            compact.push(format!("{}?", summary.unconfirmed));
        }
        if summary.unread > 0 {
            compact.push(format!("+{}", summary.unread));
        }
        self.compact_summary = compact.join(" ");
    }

    #[inline]
    pub fn has_tasks(&self) -> bool {
        !self.footer_summary.is_empty()
    }
    #[inline]
    pub fn summary(&self) -> TaskSummary {
        self.summary
    }
    #[inline]
    pub fn is_panel(&self) -> bool {
        matches!(
            self.focus,
            TaskFocus::List | TaskFocus::Detail | TaskFocus::Ttl
        )
    }

    /// The cache-TTL advice and whether its tip is a footer stop.
    pub fn set_cache_advice(&mut self, advice: Option<TtlAdvice>, actionable: bool) {
        self.tip_actionable = actionable && advice.is_some();
        self.ttl_advice = advice;
        self.clamp();
    }

    /// Whether ↓ from an empty prompt has anything in the footer to reach.
    pub fn has_footer_stop(&self) -> bool {
        self.has_tasks() || self.tip_actionable
    }

    /// Focus the first footer stop: the task label, else the tip.
    pub fn focus_footer(&mut self) {
        if self.has_tasks() {
            self.focus = TaskFocus::Footer;
        } else if self.tip_actionable {
            self.focus = TaskFocus::Tip;
        }
    }

    /// ←/→ between the footer's stops.
    pub fn move_footer_stop(&mut self, right: bool) {
        match (self.focus, right) {
            (TaskFocus::Footer, true) if self.tip_actionable => self.focus = TaskFocus::Tip,
            (TaskFocus::Tip, false) if self.has_tasks() => self.focus = TaskFocus::Footer,
            _ => {}
        }
    }

    /// Open the cache-TTL panel on the suggested row, or on `auto` while the
    /// TTL follows the session's stats.
    pub fn open_ttl(&mut self) {
        let Some(advice) = &self.ttl_advice else {
            return;
        };
        let suggested = if advice.auto {
            TtlChoice::Auto
        } else if advice.suggested_seconds == 3600 {
            TtlChoice::OneHour
        } else {
            TtlChoice::FiveMinutes
        };
        self.ttl_row = TTL_CHOICES
            .iter()
            .position(|choice| *choice == suggested)
            .unwrap_or(0);
        self.focus = TaskFocus::Ttl;
    }

    /// Leave the panel for the tip it was opened from.
    pub fn close_ttl(&mut self) {
        self.focus = TaskFocus::Tip;
        self.clamp();
    }

    pub fn move_ttl(&mut self, down: bool) {
        self.ttl_row = if down {
            (self.ttl_row + 1).min(TTL_CHOICES.len() - 1)
        } else {
            self.ttl_row.saturating_sub(1)
        };
    }

    pub fn ttl_choice(&self) -> TtlChoice {
        TTL_CHOICES[self.ttl_row.min(TTL_CHOICES.len() - 1)]
    }
    pub fn shows_running_detail(&self) -> bool {
        self.focus == TaskFocus::Detail
            && self.selected.as_ref().is_some_and(|id| {
                self.entries
                    .get(id)
                    .is_some_and(|entry| entry.task.status.is_active())
            })
    }
    #[inline]
    pub fn close(&mut self) {
        self.focus = TaskFocus::Input;
    }

    pub fn open(&mut self) {
        self.focus = TaskFocus::List;
        self.clamp();
    }
    fn clamp(&mut self) {
        if !self
            .selected
            .as_ref()
            .is_some_and(|id| self.rows.contains(id))
        {
            self.selected = self.rows.first().cloned();
        }
        if !self.has_tasks() && self.focus == TaskFocus::Footer {
            self.close();
        }
        if !self.tip_actionable && self.focus == TaskFocus::Tip
            || self.ttl_advice.is_none() && self.focus == TaskFocus::Ttl
        {
            self.close();
        }
    }

    pub fn move_selection(&mut self, down: bool) {
        let current = self
            .selected
            .as_ref()
            .and_then(|id| self.rows.iter().position(|row| row == id))
            .unwrap_or(0);
        let next = if down {
            (current + 1).min(self.rows.len().saturating_sub(1))
        } else {
            current.saturating_sub(1)
        };
        self.selected = self.rows.get(next).cloned();
    }

    pub fn detail(&mut self) {
        if let Some(entry) = self
            .selected
            .as_ref()
            .and_then(|id| self.entries.get_mut(id))
        {
            entry.read = true;
            self.focus = TaskFocus::Detail;
            self.scroll = 0;
            self.refresh_summary();
        }
    }
    pub fn scroll(&mut self, up: bool, amount: usize) {
        if up {
            self.scroll = self.scroll.saturating_add(amount);
        } else {
            self.scroll = self.scroll.saturating_sub(amount);
        }
    }
    pub fn stop_id(&self) -> Option<String> {
        self.selected
            .as_ref()
            .filter(|id| self.entries[*id].task.status.is_active())
            .cloned()
    }

    pub fn footer_label(&self, compact: bool) -> String {
        if !self.has_tasks() {
            return String::new();
        }
        let summary = if compact {
            &self.compact_summary
        } else {
            &self.footer_summary
        };
        if self.focus == TaskFocus::Footer {
            format!("{summary} · Enter to view")
        } else {
            summary.clone()
        }
    }

    /// Project inside InputSlot's allocation, not an independent terminal budget.
    pub fn panel(&mut self, width: usize, max_rows: usize) -> String {
        if self.focus == TaskFocus::Ttl {
            return self.ttl_panel(width);
        }
        let mut lines = vec![format!(
            "{}Background tasks{RESET}",
            ansi_fg(theme().primary)
        )];
        if self.focus == TaskFocus::List {
            let _ = write!(
                lines[0],
                "{} · {} in this session{RESET}",
                theme().muted_fg(),
                self.rows.len()
            );
            let rows = max_rows.saturating_sub(2).clamp(1, 18);
            if self.rows.is_empty() {
                lines.push("  No background tasks in this session".into());
            }
            let selected = self
                .selected
                .as_ref()
                .and_then(|id| self.rows.iter().position(|row| row == id))
                .unwrap_or(0);
            let start = selected
                .saturating_sub(rows / 2)
                .min(self.rows.len().saturating_sub(rows));
            for id in self.rows.iter().skip(start).take(rows) {
                let entry = &self.entries[id];
                let task = &entry.task;
                let focused = self.selected.as_ref() == Some(id);
                let marker = if focused { "❯" } else { " " };
                let marker =
                    ChoiceText::new(theme(), crate::render::ColorSupport::detect(), focused)
                        .prefix(marker);
                let kind = match task.kind {
                    BackgroundTaskKind::Terminal => "terminal",
                    BackgroundTaskKind::Agent => "agent",
                };
                let status = format!(
                    "{}{}{}",
                    ansi_fg(status_color(task.status)),
                    task.status.label(),
                    RESET
                );
                let title = fit(&plain(&task.title), width.saturating_sub(28));
                lines.push(format!(
                    "{marker} {}{title}{RESET} · {kind} · {status}{}",
                    theme().peer_sender_fg(),
                    if !entry.read && !task.status.is_active() {
                        " · new"
                    } else {
                        ""
                    }
                ));
            }
            lines.push(format!(
                "{}↑/↓ select · Enter details · k stop · Esc back{RESET}",
                theme().muted_fg()
            ));
        } else if let Some(entry) = self
            .selected
            .as_ref()
            .and_then(|id| self.entries.get_mut(id))
        {
            lines.extend(entry.detail_panel(width, max_rows, &mut self.scroll));
        }
        lines
            .into_iter()
            .map(|line| truncate_to_width(&line, width))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Entry {
    /// Format the inspected task from producer state and cached output.
    fn detail_panel(&mut self, width: usize, max_rows: usize, scroll: &mut usize) -> Vec<String> {
        let task = &self.task;
        let support = crate::render::ColorSupport::detect();
        let muted = ansi_fg(support.color(theme().muted));
        let border = ansi_fg(support.color(theme().border));
        let compact = max_rows
            < 7 + usize::from(task.command.is_some())
                + usize::from(!task.activity.is_empty())
                + usize::from(task.output_path.is_some());
        let kind = match task.kind {
            BackgroundTaskKind::Terminal => "Shell details",
            BackgroundTaskKind::Agent => "Agent details",
        };
        let mut lines = vec![format!(
            "{}{kind}{RESET} · {}{}{RESET}",
            ansi_fg(support.color(theme().primary)),
            theme().peer_sender_fg(),
            fit(&plain(&task.title), width.saturating_sub(kind.len() + 3)),
        )];
        let elapsed = task.elapsed_ms.saturating_add(if task.status.is_active() {
            u64::try_from(self.received.elapsed().as_millis()).unwrap_or(u64::MAX)
        } else {
            0
        });
        let runtime = runtime_label(elapsed / 1000);
        let exit = task
            .exit_code
            .map_or(String::new(), |code| format!(" · exit {code}"));
        let mut status = format!(
            "{muted}Status:   {}{}{exit}{RESET}",
            ansi_fg(support.color(status_color(task.status))),
            task.status.label(),
        );
        if compact {
            let _ = write!(status, "{muted} · {runtime}{RESET}");
        }
        lines.push(status);
        if !compact {
            lines.push(format!("{muted}Runtime:  {RESET}{runtime}"));
        }
        if let Some(command) = &task.command {
            let command_width = width.saturating_sub(10).max(1);
            // Cache at most three command rows across output and clock updates.
            if self.command_rows.as_ref().map(|(w, _)| *w) != Some(command_width) {
                let text = StyledText::from_ansi(&fit(
                    &plain(command),
                    command_width.saturating_mul(COMMAND_PREVIEW_ROWS),
                ));
                let wrapped = wrap_line(&text, 0..text.text.len(), command_width, 0);
                let clipped = wrapped.len() > COMMAND_PREVIEW_ROWS;
                let mut rows: Vec<_> = wrapped
                    .into_iter()
                    .take(COMMAND_PREVIEW_ROWS)
                    .map(|(row, _)| row.text)
                    .collect();
                if clipped {
                    let last = &mut rows[COMMAND_PREVIEW_ROWS - 1];
                    *last = fit(&format!("{last}…"), command_width);
                }
                self.command_rows = Some((command_width, rows));
            }
            let rows = &self.command_rows.as_ref().expect("command rows prepared").1;
            let command_budget = max_rows
                .saturating_sub(
                    8 + usize::from(!task.activity.is_empty())
                        + usize::from(task.output_path.is_some()),
                )
                .clamp(1, COMMAND_PREVIEW_ROWS);
            let count = rows.len().min(command_budget);
            for (index, row) in rows.iter().take(count).enumerate() {
                let label = if index == 0 {
                    "Command:  "
                } else {
                    "          "
                };
                let row = if index + 1 == count && count < rows.len() {
                    fit(&format!("{row}…"), command_width)
                } else {
                    row.clone()
                };
                lines.push(format!("{muted}{label}{RESET}{row}"));
            }
        }
        if !task.activity.is_empty() {
            lines.push(format!("{muted}Activity: {RESET}{}", plain(&task.activity)));
        }

        // The shared InputSlot allocator owns height. Borders yield first in
        // a short window; controls and the full-output path remain outside it.
        let trailing = usize::from(task.output_path.is_some()) + 1;
        let framed = width >= 12 && max_rows >= 1 + lines.len() + trailing + 3;
        let captioned = framed || max_rows >= 1 + lines.len() + trailing + 2;
        // Desired-height measurement and the allocated projection share one
        // wrap width, even when the latter has to drop its frame.
        let content_width = width.saturating_sub(if width >= 12 { 6 } else { 2 }).max(1);
        let output = self
            .output
            .get_or_insert_with(|| StyledText::from_ansi(&task.output));
        if self.wrapped_width != content_width {
            self.wrapped.clear();
            let mut offset = 0;
            // A final newline terminates a record; internal blank lines count.
            for line in output
                .text
                .strip_suffix('\n')
                .unwrap_or(&output.text)
                .split('\n')
            {
                self.wrapped.extend(
                    wrap_line(output, offset..offset + line.len(), content_width, 0)
                        .into_iter()
                        .map(|(row, _)| row),
                );
                offset += line.len() + 1;
            }
            self.wrapped_width = content_width;
        }
        let rows = max_rows
            .saturating_sub(
                1 + lines.len() + trailing + if framed { 2 } else { usize::from(captioned) },
            )
            .clamp(1, 18);
        let visible_scroll = (*scroll).min(self.wrapped.len().saturating_sub(rows));
        // The first projection measures desired height before allocation. It
        // must not change navigation using a viewport the user never sees.
        if max_rows != usize::MAX {
            *scroll = visible_scroll;
        }
        let end = self.wrapped.len().saturating_sub(visible_scroll);
        let start = end.saturating_sub(rows);
        let label = if output.text.is_empty() {
            "Output".to_string()
        } else {
            format!(
                "Output tail · rows {}–{end}/{}",
                start + 1,
                self.wrapped.len()
            )
        };
        if framed {
            let label = fit(&label, width.saturating_sub(5));
            lines.push(format!(
                "{RESET}{border}╭─ {label} {}╮{RESET}",
                "─".repeat(width.saturating_sub(display_width(&label) + 5)),
            ));
        } else if captioned {
            lines.push(format!("{muted}{label}{RESET}"));
        }
        for row in &self.wrapped[start..end] {
            // Reset both sides: bold/colored output must not style the frame.
            let mut line = if framed {
                format!("{RESET}{border}│{RESET}  ")
            } else {
                String::from("  ")
            };
            if output.text.is_empty() {
                let message = if task.status.is_active() {
                    "Waiting for output…"
                } else {
                    "No output"
                };
                let _ = write!(line, "{muted}{}{RESET}", fit(message, content_width));
                if framed {
                    line.push_str(
                        &" ".repeat(content_width.saturating_sub(display_width(message))),
                    );
                }
            } else {
                row.write_ansi(0..row.text.len(), &mut line);
                line.push_str(RESET);
                if framed {
                    line.push_str(
                        &" ".repeat(content_width.saturating_sub(display_width(&row.text))),
                    );
                }
            }
            if framed {
                let _ = write!(line, "  {border}│{RESET}");
            }
            lines.push(line);
        }
        if framed {
            lines.push(format!("{RESET}{border}╰{}╯{RESET}", "─".repeat(width - 2)));
        }
        if let Some(path) = &task.output_path {
            lines.push(format!(
                "{muted}{}{RESET}",
                fit(&format!("Full output: {path}"), width)
            ));
        }
        lines.push(format!(
            "{muted}↑/↓ scroll{} · Esc task list{RESET}",
            if task.status.is_active() {
                " · k stop"
            } else {
                ""
            },
        ));
        lines
    }
}

fn runtime_label(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}h {}m {}s",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

impl TaskBrowser {
    /// The cache-TTL choices with what each one does and costs, from the
    /// advisor's saved decision. Reference prices, not an invoice.
    fn ttl_panel(&self, width: usize) -> String {
        let Some(advice) = &self.ttl_advice else {
            return String::new();
        };
        let muted = theme().muted_fg();
        let mode = if advice.auto {
            let target = advice.auto_target();
            if target == advice.effective_seconds {
                " · auto".to_owned()
            } else {
                format!(" · auto, heading for {}", ttl_label(target))
            }
        } else {
            advice.pending.map_or(" · pinned".to_owned(), |pending| {
                format!(" · pinned, pending {}", choice_label(pending.choice))
            })
        };
        let mut lines = vec![format!(
            "{}Prompt cache TTL{RESET}{muted} · now {}{mode}{RESET}",
            ansi_fg(theme().primary),
            ttl_label(advice.effective_seconds),
        )];
        if advice.has_saving {
            lines.push(format!(
                "{muted}  advisor: {} ≈ {:.0}% cheaper over the last {} requests (${:.2} at 5m vs ${:.2} at 1h, reference prices){RESET}",
                ttl_label(advice.suggested_seconds),
                advice.saving_pct(),
                advice.rows,
                advice.fixed_5m_usd,
                advice.fixed_1h_usd,
            ));
        }
        for (row, choice) in TTL_CHOICES.iter().enumerate() {
            let focused = row == self.ttl_row;
            let marker = ChoiceText::new(theme(), crate::render::ColorSupport::detect(), focused)
                .prefix(if focused { "❯" } else { " " });
            // While automatic, choosing 1h or 5m pins it.
            let pin = if advice.auto { "pin · " } else { "" };
            let detail = match choice {
                TtlChoice::OneHour if advice.effective_seconds == 3600 => format!("{pin}current"),
                TtlChoice::OneHour => {
                    let cost = match (advice.switch_now_usd, advice.switch_cold_usd) {
                        (Some(now), Some(cold)) if advice.cached_tokens > 0 => format!(
                            " · switching now rewrites {} cached tokens once (~${now:.2}; ~${cold:.2} when cold)",
                            tokens(advice.cached_tokens)
                        ),
                        _ => String::new(),
                    };
                    format!(
                        "{pin}at the next cold start (idle past the TTL, compact, resume){cost}"
                    )
                }
                TtlChoice::FiveMinutes if advice.effective_seconds == 300 => {
                    format!("{pin}current")
                }
                TtlChoice::FiveMinutes => format!("{pin}from the next request · keeps the cache"),
                TtlChoice::Auto if advice.auto => {
                    format!(
                        "current · follows this session's stats: {}",
                        auto_verdict(advice)
                    )
                }
                TtlChoice::Auto => {
                    format!("follow this session's stats: {}", auto_verdict(advice))
                }
            };
            lines.push(format!(
                "{marker} {:<4} {muted}{detail}{RESET}",
                choice_label(*choice)
            ));
        }
        lines.push(format!(
            "{muted}↑/↓ select · Enter apply and save · n apply now · Esc back{RESET}"
        ));
        lines
            .into_iter()
            .map(|line| truncate_to_width(&line, width))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What the stats say, for the `auto` row.
fn auto_verdict(advice: &TtlAdvice) -> String {
    if !advice.has_evidence {
        format!(
            "collecting {}/{MIN_INTERVALS} reuse intervals, 5m meanwhile",
            advice.intervals
        )
    } else if advice.has_saving {
        format!(
            "{} ≈{:.0}% cheaper here",
            ttl_label(advice.suggested_seconds),
            advice.saving_pct()
        )
    } else {
        "5m and 1h within 10% here".to_owned()
    }
}

fn choice_label(choice: TtlChoice) -> &'static str {
    match choice {
        TtlChoice::OneHour => "1h",
        TtlChoice::FiveMinutes => "5m",
        TtlChoice::Auto => "auto",
    }
}

fn tokens(count: u64) -> String {
    if count >= 1000 {
        format!("{}k", count / 1000)
    } else {
        count.to_string()
    }
}

fn status_color(status: BackgroundTaskStatus) -> crossterm::style::Color {
    match status {
        BackgroundTaskStatus::Running => theme().info(),
        BackgroundTaskStatus::Stopping | BackgroundTaskStatus::ExecutionUnconfirmed => {
            theme().warning
        }
        BackgroundTaskStatus::Completed => theme().tool_borders().success,
        BackgroundTaskStatus::Failed | BackgroundTaskStatus::Cancelled => theme().error,
    }
}
fn plain(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .map(|ch| if matches!(ch, '\n' | '\t') { ' ' } else { ch })
        .collect()
}
fn fit(text: &str, width: usize) -> String {
    truncate_to_width(&text.replace(['\n', '\r'], " "), width)
}

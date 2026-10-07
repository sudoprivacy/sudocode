//! Task browser with bounded, lazily decoded output and event-driven repaint.
use crate::render::{
    ansi_fg,
    styled_text::StyledText,
    text_layout::{display_width, truncate_to_width, wrap_line},
    theme, RESET,
};
use engine_events::{
    BackgroundTask, BackgroundTaskEvent, BackgroundTaskKind, BackgroundTaskStatus,
};
use std::{collections::BTreeMap, time::Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum TaskFocus {
    #[default]
    Input,
    Footer,
    List,
    Detail,
}

struct Entry {
    task: BackgroundTask,
    output: Option<StyledText>,
    read: bool,
    received: Instant,
    wrapped_width: usize,
    wrapped: Vec<StyledText>,
}

#[derive(Default)]
pub(super) struct TaskBrowser {
    entries: BTreeMap<String, Entry>,
    rows: Vec<String>,
    summary: String,
    pub focus: TaskFocus,
    selected: Option<String>,
    scroll: usize,
}

impl TaskBrowser {
    /// Apply producer state and report whether the visible browser changed.
    pub fn update(&mut self, event: BackgroundTaskEvent) -> bool {
        let previous_summary = self.summary.clone();
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
        let mut terminals = 0;
        let mut agents = 0;
        let mut unread = 0;
        for id in &self.rows {
            let entry = &self.entries[id];
            if entry.task.status.is_active() {
                match entry.task.kind {
                    BackgroundTaskKind::Terminal => terminals += 1,
                    BackgroundTaskKind::Agent => agents += 1,
                }
            } else if !entry.read {
                unread += 1;
            }
        }
        let mut parts = Vec::new();
        if terminals > 0 {
            parts.push(format!(
                "{terminals} terminal{}",
                if terminals == 1 { "" } else { "s" }
            ));
        }
        if agents > 0 {
            parts.push(format!(
                "{agents} agent{}",
                if agents == 1 { "" } else { "s" }
            ));
        }
        if unread > 0 {
            parts.push(format!(
                "{unread} new result{}",
                if unread == 1 { "" } else { "s" }
            ));
        }
        self.summary = parts.join(" · ");
    }

    #[inline]
    pub fn has_tasks(&self) -> bool {
        !self.summary.is_empty()
    }
    #[inline]
    pub fn is_panel(&self) -> bool {
        matches!(self.focus, TaskFocus::List | TaskFocus::Detail)
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

    pub fn footer(&self, base: &str, width: usize) -> String {
        if !self.has_tasks() {
            return fit(base, width);
        }
        let compact = &self.summary;
        let label = format!(
            "{compact} · {}",
            if self.focus == TaskFocus::Footer {
                "Enter to view"
            } else {
                "↓ to view"
            }
        );
        let available = width.saturating_sub(display_width(&label) + 3);
        let left = fit(base, available);
        let separator = if left.is_empty() { "" } else { " · " };
        let (prefix, suffix) = if self.focus == TaskFocus::Footer {
            ("\x1b[7m", RESET)
        } else {
            ("", "")
        };
        let label = fit(
            &label,
            width.saturating_sub(display_width(&left) + display_width(separator)),
        );
        format!(
            "{left}{separator}{}{prefix}{label}{suffix}{RESET}",
            ansi_fg(theme().muted)
        )
    }

    pub fn panel(&mut self, width: usize, height: usize) -> String {
        let rows = height.saturating_sub(12).clamp(1, 18);
        let mut lines = vec![format!(
            "{}Background tasks{RESET}",
            ansi_fg(theme().primary)
        )];
        if self.focus == TaskFocus::List {
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
                let marker = if self.selected.as_ref() == Some(id) {
                    "❯"
                } else {
                    " "
                };
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
            let task = &entry.task;
            lines.push(format!(
                "{}{}{RESET} · {}{}{RESET}",
                theme().peer_sender_fg(),
                fit(&plain(&task.title), width.saturating_sub(18)),
                ansi_fg(status_color(task.status)),
                task.status.label()
            ));
            if let Some(command) = &task.command {
                lines.push(fit(&format!("$ {}", plain(command)), width));
            }
            let details = format!(
                "{}s{}{}",
                (task.elapsed_ms.saturating_add(if task.status.is_active() {
                    u64::try_from(entry.received.elapsed().as_millis()).unwrap_or(u64::MAX)
                } else {
                    0
                })) / 1000,
                task.exit_code
                    .map_or(String::new(), |code| format!(" · exit {code}")),
                if task.activity.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", plain(&task.activity))
                }
            );
            lines.push(format!("{}{details}{RESET}", theme().muted_fg()));
            let content_width = width.saturating_sub(2).max(1);
            // Hidden output stays raw and bounded. Decode only when inspecting
            // this task, then reuse it across keys, ticks and resizes.
            let output = entry
                .output
                .get_or_insert_with(|| StyledText::from_ansi(&task.output));
            if entry.wrapped_width != content_width {
                entry.wrapped.clear();
                let mut offset = 0;
                for line in output.text.split('\n') {
                    entry.wrapped.extend(
                        wrap_line(output, offset..offset + line.len(), content_width, 0)
                            .into_iter()
                            .map(|(row, _)| row),
                    );
                    offset += line.len() + 1;
                }
                entry.wrapped_width = content_width;
            }
            let output_lines = &entry.wrapped;
            let end = output_lines
                .len()
                .saturating_sub(self.scroll.min(output_lines.len().saturating_sub(1)));
            let start = end.saturating_sub(rows);
            if output.text.is_empty() {
                lines.push(format!(
                    "{}  Waiting for output…{RESET}",
                    theme().muted_fg()
                ));
            } else {
                lines.extend(output_lines[start..end].iter().map(|line| {
                    let mut out = String::from("  ");
                    line.write_ansi(0..line.text.len(), &mut out);
                    out.push_str(RESET);
                    out
                }));
            }
            if let Some(path) = &task.output_path {
                lines.push(format!(
                    "{}{}{RESET}",
                    theme().muted_fg(),
                    fit(&format!("Full output: {path}"), width)
                ));
            }
            lines.push(format!(
                "{}↑/↓ scroll · k stop · Esc task list{RESET}",
                theme().muted_fg()
            ));
        }
        lines
            .into_iter()
            .map(|line| truncate_to_width(&line, width))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn status_color(status: BackgroundTaskStatus) -> crossterm::style::Color {
    match status {
        BackgroundTaskStatus::Running => theme().info(),
        BackgroundTaskStatus::Stopping => theme().warning,
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

//! Bounded peer navigation. Engine discovery owns identity and zone presence.
use crate::{
    cli::format::agent_row,
    render::{text_layout::truncate_to_width, theme, TextRole, RESET},
};
use serde_json::Value;

#[derive(Default)]
pub(super) struct PeerTab {
    rows: Vec<(String, String)>,
    selected: usize,
    notice: Option<String>,
    loaded: bool,
}

impl PeerTab {
    pub fn update(&mut self, result: Result<Value, String>) {
        let previous = self.rows.get(self.selected).map(|row| row.0.clone());
        self.rows.clear();
        self.loaded = true;
        match result {
            Ok(value) => {
                self.notice = None;
                if let Some(rows) = value.get("agents").and_then(Value::as_array) {
                    for row in rows.iter().filter(|row| row["kind"] == "peer") {
                        if let Some(label) = agent_row(row) {
                            let key = row["send_to"].as_str().unwrap_or_default().to_string();
                            self.rows.push((key, label));
                        }
                    }
                }
            }
            Err(error) => self.notice = Some(format!("Directory unavailable: {error}")),
        }
        self.selected = previous
            .and_then(|key| self.rows.iter().position(|row| row.0 == key))
            .unwrap_or(0);
    }

    pub fn move_selection(&mut self, down: bool) {
        self.selected = if down {
            (self.selected + 1).min(self.rows.len().saturating_sub(1))
        } else {
            self.selected.saturating_sub(1)
        };
    }

    pub fn panel(&self, width: usize, max_rows: usize, detail: bool) -> String {
        let theme = theme();
        let mode = if detail { " · details" } else { "" };
        let mut lines = vec![format!(
            "{}Peers{}{mode} · snapshot · r refresh",
            theme.fg_seq(TextRole::Brand),
            RESET
        )];
        if let Some(notice) = &self.notice {
            lines.push(format!(
                "{}{notice}{RESET}",
                theme.fg_seq(TextRole::Warning)
            ));
        }
        let room = max_rows.saturating_sub(lines.len() + 1).min(18);
        if !self.loaded {
            lines.push("  Reading agent directory…".into());
        } else if self.rows.is_empty() {
            lines.push("  No peers in this snapshot".into());
        } else if detail {
            if let Some((_, label)) = self.rows.get(self.selected) {
                lines.extend(
                    crate::render::text_layout::wrap_ansi_to_width(label, width.saturating_sub(2))
                        .into_iter()
                        .take(room)
                        .map(|line| format!("  {line}")),
                );
            }
        } else {
            let start = self
                .selected
                .saturating_sub(room / 2)
                .min(self.rows.len().saturating_sub(room));
            for (index, (_, label)) in self.rows.iter().enumerate().skip(start).take(room) {
                lines.push(format!(
                    "{} {label}",
                    if index == self.selected { "❯" } else { " " }
                ));
            }
        }
        lines.push(format!(
            "{}↑/↓ select · Enter details · ←/→ tabs · r refresh · Esc back{RESET}",
            theme.fg_seq(TextRole::Secondary)
        ));
        lines
            .into_iter()
            .take(max_rows)
            .map(|line| truncate_to_width(&line, width))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

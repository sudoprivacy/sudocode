//! Allocate live slots inside one terminal row budget. No terminal writes.
//!
//! The same structured document and iocraft text primitive measure and paint
//! each slot. Only current-frame measurements survive; history is not cached.

use std::{collections::HashMap, sync::Arc};

use iocraft::prelude::*;

use super::ansi_text::RichText;
use crate::render::styled_text::StyledText;

#[derive(Default)]
pub(super) struct Measurements {
    width: usize,
    rows: HashMap<String, (usize, bool)>,
}

impl Measurements {
    pub fn begin(&mut self, width: usize) {
        if self.width != width {
            self.rows.clear();
            self.width = width;
        }
        for (_, used) in self.rows.values_mut() {
            *used = false;
        }
    }

    pub fn rows(&mut self, content: &Arc<StyledText>) -> usize {
        if content.text.is_empty() {
            return 0;
        }
        // Attributes affect appearance, not text wrapping or row count.
        if let Some((rows, used)) = self.rows.get_mut(&content.text) {
            *used = true;
            return *rows;
        }
        let rows = element! { RichText(content: content.clone()) }
            .render(Some(self.width.max(1)))
            .height();
        self.rows.insert(content.text.clone(), (rows, true));
        rows
    }

    pub fn end(&mut self) {
        self.rows.retain(|_, (_, used)| *used);
    }
}

pub(super) fn ansi(text: impl AsRef<str>) -> Arc<StyledText> {
    Arc::new(StyledText::from_ansi(text.as_ref()))
}

pub(super) struct Slot {
    pub full: Arc<StyledText>,
    pub summary: Arc<StyledText>,
}

pub(super) struct Placement {
    pub content: Arc<StyledText>,
    pub rows: usize,
}

impl Placement {
    fn measured(content: Arc<StyledText>, measurements: &mut Measurements) -> Self {
        let rows = measurements.rows(&content);
        Self { content, rows }
    }
}

pub(super) struct ChromeLayout {
    pub pending: Placement,
    pub status: Placement,
    pub todo: Placement,
    pub footer: Placement,
    pub input_rows: usize,
    pub separators: usize,
    pub warning: Option<String>,
}

/// Keep interactive controls outside the scrolling body. If even fixed
/// controls do not fit, the caller shows a warning and disables confirmation.
#[derive(Default)]
pub(super) struct InputLayout {
    pub heading: usize,
    pub body: usize,
    pub hint: usize,
    pub controls: usize,
    pub editor: usize,
    pub fits_controls: bool,
}

impl InputLayout {
    pub fn allocate(
        available: usize,
        [heading, body, controls, editor]: [usize; 4],
        measurements: &mut Measurements,
    ) -> Self {
        let fixed = heading + controls + editor;
        let capacity = available.saturating_sub(fixed);
        // Reserve the largest label so paging cannot change the layout.
        let hint = if body > capacity {
            measurements.rows(&ansi(review_hint(body - 1, 1, body)))
        } else {
            0
        };
        let visible_body = body.min(capacity.saturating_sub(hint));
        Self {
            heading,
            body: visible_body,
            hint,
            controls,
            editor: available.saturating_sub(heading + visible_body + hint + controls),
            fits_controls: fixed <= available && (body == 0 || visible_body > 0),
        }
    }
}

pub(super) fn review_hint(offset: usize, visible: usize, total: usize) -> String {
    format!(
        "Review {}-{}/{total} · PgUp/PgDn · Ctrl+Home/End",
        offset + 1,
        offset + visible,
    )
}

impl ChromeLayout {
    pub fn allocate(
        width: usize,
        height: usize,
        desired_input: usize,
        slots: [Slot; 4],
        combined_summary: String,
        measurements: &mut Measurements,
    ) -> Self {
        // Leave one row outside the live region; this is not a cursor offset.
        let available = height.saturating_sub(1);
        let full_rows: usize = slots.iter().map(|s| measurements.rows(&s.full)).sum();
        let desired_input = desired_input.max(1);
        let full = width >= 16 && full_rows + desired_input + 2 <= available;
        let [pending, status, todo, footer] = slots
            .map(|s| if full { s.full } else { s.summary })
            .map(|text| Placement::measured(text, measurements));
        let mut layout = Self {
            pending,
            status,
            todo,
            footer,
            input_rows: desired_input,
            separators: 1,
            warning: None,
        };
        let overhead =
            layout.pending.rows + layout.status.rows + layout.todo.rows + layout.footer.rows + 2;
        if overhead < available && width >= 16 {
            layout.input_rows = desired_input.min(available - overhead);
            return layout;
        }

        // Explicit counts replace even the per-slot summaries, not state.
        layout.pending = Placement::measured(ansi(combined_summary), measurements);
        layout.status = Placement::measured(Default::default(), measurements);
        layout.todo = Placement::measured(Default::default(), measurements);
        layout.footer = Placement::measured(Default::default(), measurements);
        layout.separators = 0;
        if width >= 16 && layout.pending.rows < available {
            layout.input_rows = desired_input.min(available - layout.pending.rows);
        } else {
            layout.pending = Placement::measured(Default::default(), measurements);
            layout.input_rows = available;
            layout.warning = Some("Enlarge terminal".into());
        }
        layout
    }
}

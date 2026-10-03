//! Row allocation for live chrome. No terminal writes or application state.
//!
//! Measure with the renderer itself so ANSI, Unicode and word wrapping have
//! one implementation. Retain only measurements used by the current frame;
//! this is not a transcript cache and typing does not remeasure stable slots.

use std::collections::HashMap;

use iocraft::prelude::*;

use super::ansi_text::AnsiText;

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

    pub fn rows(&mut self, content: &str) -> usize {
        if content.is_empty() {
            return 0;
        }
        if let Some((rows, used)) = self.rows.get_mut(content) {
            *used = true;
            return *rows;
        }
        let rows = element! { AnsiText(content: content.to_owned()) }
            .render(Some(self.width.max(1)))
            .height();
        self.rows.insert(content.to_owned(), (rows, true));
        rows
    }

    pub fn end(&mut self) {
        self.rows.retain(|_, (_, used)| *used);
    }
}

pub(super) struct Slot {
    pub full: String,
    pub summary: String,
}

pub(super) struct Placement {
    pub text: String,
    pub rows: usize,
}

impl Placement {
    fn measured(text: String, measurements: &mut Measurements) -> Self {
        let rows = measurements.rows(&text);
        Self { text, rows }
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

/// Allocate within InputSlot after the shared chrome budget. Interactive
/// controls never scroll out of view; only the question body can scroll.
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
        // Heading, complete body, controls, minimum editor rows.
        [heading, body, controls, editor]: [usize; 4],
        measurements: &mut Measurements,
    ) -> Self {
        let fixed = heading + controls + editor;
        let capacity = available.saturating_sub(fixed);
        // Reserve the largest range label so paging cannot change layout.
        let hint = if body > capacity {
            measurements.rows(&review_hint(body - 1, 1, body))
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
        // Keep one row outside the live frame. This is a size constraint, not
        // an assumption about where terminal reflow leaves the cursor.
        let available = height.saturating_sub(1);
        let full_rows: usize = slots.iter().map(|s| measurements.rows(&s.full)).sum();
        let desired_input = desired_input.max(1);
        let full = width >= 16 && full_rows + desired_input + 2 <= available;
        let texts = slots.map(|s| if full { s.full } else { s.summary });
        let [pending, status, todo, footer] =
            texts.map(|text| Placement::measured(text, measurements));
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

        // Even the per-slot summaries do not fit. Combine counts explicitly;
        // preserve the same InputSlot rather than remounting an emergency UI.
        layout.pending = Placement::measured(combined_summary, measurements);
        layout.status = Placement::measured(String::new(), measurements);
        layout.todo = Placement::measured(String::new(), measurements);
        layout.footer = Placement::measured(String::new(), measurements);
        layout.separators = 0;
        if width >= 16 && layout.pending.rows < available {
            layout.input_rows = desired_input.min(available - layout.pending.rows);
        } else {
            layout.pending = Placement::measured(String::new(), measurements);
            layout.input_rows = available;
            layout.warning = Some("Enlarge terminal".into());
        }
        layout
    }
}

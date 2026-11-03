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
        // UTF-8 bytes bound the painted columns from above (the adapter
        // removes tabs/CR). A fitting logical line needs no temporary canvas.
        // Wider or multiline text still uses the actual iocraft primitive.
        if content.text.len() <= self.width.max(1)
            && !content.text.contains([
                '\n', '\r', '\u{000b}', '\u{000c}', '\u{0085}', '\u{2028}', '\u{2029}',
            ])
        {
            return 1;
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
    ansi_with_style(text, crossterm::style::ContentStyle::default())
}

pub(super) fn ansi_with_style(
    text: impl AsRef<str>,
    initial_style: crossterm::style::ContentStyle,
) -> Arc<StyledText> {
    Arc::new(StyledText::from_ansi_with_style(
        text.as_ref(),
        initial_style,
    ))
}

pub(super) struct Slot {
    pub minimum_full_rows: usize,
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
    pub preview_rows: usize,
    pub separators: usize,
    pub warning: Option<String>,
}

/// Reserve visible decision controls before the editor. If they do not fit,
/// the caller shows a warning and disables confirmation.
#[derive(Default)]
pub(super) struct InputLayout {
    pub heading: usize,
    pub controls: usize,
    pub editor: usize,
    pub fits_controls: bool,
}

impl InputLayout {
    pub fn allocate(available: usize, [heading, controls, editor]: [usize; 3]) -> Self {
        let fixed = heading + controls + editor;
        Self {
            heading,
            controls,
            editor: available.saturating_sub(heading + controls),
            fits_controls: fixed <= available,
        }
    }
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
        let full_rows: usize = slots
            .iter()
            .map(|s| measurements.rows(&s.full).max(s.minimum_full_rows))
            .sum();
        let desired_input = desired_input.max(1);
        let full = width >= 16 && full_rows + desired_input + 2 <= available;
        let [pending, status, todo, footer] = slots.map(|slot| {
            let mut placed =
                Placement::measured(if full { slot.full } else { slot.summary }, measurements);
            if full {
                placed.rows = placed.rows.max(slot.minimum_full_rows);
            }
            placed
        });
        let mut layout = Self {
            pending,
            status,
            todo,
            footer,
            input_rows: desired_input,
            preview_rows: 0,
            separators: 1,
            warning: None,
        };
        let overhead =
            layout.pending.rows + layout.status.rows + layout.todo.rows + layout.footer.rows + 2;
        if overhead < available && width >= 16 {
            layout.input_rows = desired_input.min(available - overhead);
            // Prose is optional and has the lowest row priority. Keep the
            // existing extra anchor margin for concurrent status changes.
            layout.preview_rows = available.saturating_sub(overhead + layout.input_rows + 1);
            return layout;
        }

        // Explicit counts replace even the per-slot summaries, not state.
        layout.pending = Placement::measured(ansi(combined_summary), measurements);
        layout.status = Placement::measured(std::sync::Arc::default(), measurements);
        layout.todo = Placement::measured(std::sync::Arc::default(), measurements);
        layout.footer = Placement::measured(std::sync::Arc::default(), measurements);
        layout.separators = 0;
        if width >= 16 && layout.pending.rows < available {
            layout.input_rows = desired_input.min(available - layout.pending.rows);
        } else {
            layout.pending = Placement::measured(std::sync::Arc::default(), measurements);
            layout.input_rows = available;
            layout.warning = Some("Enlarge terminal".into());
        }
        layout
    }
}

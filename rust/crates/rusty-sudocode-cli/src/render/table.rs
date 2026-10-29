//! Codex table layout adapted to scode's inline StyledText boundary.
//! Source: openai/codex 970111f6cb, markdown_render.rs and table_key_value.rs.
//! Copyright 2026 OpenAI. Apache-2.0; see third_party/codex.

use super::{code_theme, styled_text::StyledText, text_layout, ColorSupport, ColorTheme};
use crossterm::style::{ContentStyle, Stylize};
use pulldown_cmark::Alignment;

const FIELD_LEADING_PADDING: usize = 1;
const FIELD_GAP: usize = 2;
const MIN_VALUE_WIDTH: usize = 3;
const MIN_ALIGNED_COMPACT_VALUE_WIDTH: usize = 12;
const MIN_ALIGNED_EXPANSIVE_VALUE_WIDTH: usize = 24;
const MIN_SCANNABLE_NARRATIVE_WIDTH: usize = 12;
const MIN_SCANNABLE_TOKEN_HEAVY_WIDTH: usize = 12;
const CRAMPED_EXPANSIVE_CELL_LINES: usize = 4;
const CATASTROPHIC_NARRATIVE_CELL_LINES: usize = 7;
const STACKED_VALUE_INDENT: usize = 2;
const CELL_PADDING: usize = 1;
const COLUMN_GAP: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableColumnKind {
    Narrative,
    TokenHeavy,
    Compact,
}
struct TableColumnMetrics {
    max_width: usize,
    header_token_width: usize,
    body_token_width: usize,
    kind: TableColumnKind,
}

#[derive(Default)]
struct Cell(StyledText);
impl Cell {
    fn new(text: &str) -> Self {
        Self(StyledText::from_ansi(text))
    }
    fn plain_text(&self) -> &str {
        &self.0.text
    }
    fn width(&self) -> usize {
        self.0
            .text
            .lines()
            .map(text_layout::display_width)
            .max()
            .unwrap_or(0)
    }
}
fn display_width(text: &str) -> usize {
    text_layout::display_width(text)
}
fn wrap_cell(cell: &Cell, width: usize) -> Vec<StyledText> {
    text_layout::wrap_words(&cell.0, width)
}

pub(super) fn render(
    headers: &[String],
    rows: &[Vec<String>],
    alignments: &[Alignment],
    width: usize,
    theme: &ColorTheme,
    support: ColorSupport,
) -> String {
    if headers.is_empty() && rows.is_empty() {
        return String::new();
    }
    let columns = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    let normalize = |row: &[String]| {
        (0..columns)
            .map(|column| Cell::new(row.get(column).map_or("", String::as_str)))
            .collect::<Vec<_>>()
    };
    let header = normalize(headers);
    let rows = rows.iter().map(|row| normalize(row)).collect::<Vec<_>>();
    let metrics = Table::collect_table_column_metrics(&header, &rows, columns);
    let available =
        width.saturating_sub(columns * CELL_PADDING * 2 + columns.saturating_sub(1) * COLUMN_GAP);
    let header_style = ContentStyle::default()
        .with(code_theme::table_header_color(
            theme.light_background,
            support,
        ))
        .bold();
    let separator_style = ContentStyle::default().with(support.color(theme.border));
    let widths = Table::compute_column_widths(&metrics, Some(available));
    let record = widths
        .as_ref()
        .is_none_or(|widths| should_render_records(&rows, widths, &metrics));
    let mut output = Vec::new();
    if record && !rows.is_empty() {
        let label_width = header.iter().map(Cell::width).max().unwrap_or(0);
        let minimum = if metrics
            .iter()
            .any(|metric| metric.kind != TableColumnKind::Compact)
        {
            MIN_ALIGNED_EXPANSIVE_VALUE_WIDTH
        } else {
            MIN_ALIGNED_COMPACT_VALUE_WIDTH
        };
        let aligned = FIELD_LEADING_PADDING + label_width + FIELD_GAP + minimum <= width;
        for (index, row) in rows.iter().enumerate() {
            for (label, value) in header.iter().zip(row) {
                if aligned {
                    let indent = FIELD_LEADING_PADDING + label_width + FIELD_GAP;
                    for (line_index, line) in
                        wrap_cell(value, width.saturating_sub(indent).max(MIN_VALUE_WIDTH))
                            .iter()
                            .enumerate()
                    {
                        let mut rendered = StyledText::default();
                        if line_index == 0 {
                            rendered.push(ContentStyle::default(), " ");
                            append_header(&mut rendered, &label.0, header_style);
                            rendered.push(
                                ContentStyle::default(),
                                &" ".repeat(label_width.saturating_sub(label.width()) + FIELD_GAP),
                            );
                        } else {
                            rendered.push(ContentStyle::default(), &" ".repeat(indent));
                        }
                        rendered.append(line);
                        output.push(rendered);
                    }
                } else {
                    for line in wrap_cell(label, width.saturating_sub(FIELD_LEADING_PADDING).max(1))
                    {
                        let mut rendered = StyledText::default();
                        rendered.push(ContentStyle::default(), " ");
                        append_header(&mut rendered, &line, header_style);
                        output.push(rendered);
                    }
                    for line in wrap_cell(value, width.saturating_sub(STACKED_VALUE_INDENT).max(1))
                    {
                        let mut rendered = StyledText::default();
                        rendered.push(ContentStyle::default(), &" ".repeat(STACKED_VALUE_INDENT));
                        rendered.append(&line);
                        output.push(rendered);
                    }
                }
            }
            if index + 1 < rows.len() {
                output.push(rule(width, '─', separator_style));
            }
        }
    } else if let Some(widths) = widths {
        output.extend(render_row(&header, &widths, alignments, Some(header_style)));
        output.push(table_rule(&widths, '━', separator_style));
        for (index, row) in rows.iter().enumerate() {
            output.extend(render_row(row, &widths, alignments, None));
            if index + 1 < rows.len() {
                output.push(table_rule(&widths, '─', separator_style));
            }
        }
    } else {
        let mut line = StyledText::default();
        for cell in &header {
            line.push(ContentStyle::default(), "| ");
            line.append(&cell.0);
            line.push(ContentStyle::default(), " ");
        }
        line.push(ContentStyle::default(), "|");
        output.extend(text_layout::wrap_words(&line, width));
    }
    let mut ansi = String::new();
    for (index, line) in output.iter().enumerate() {
        if index > 0 {
            ansi.push('\n');
        }
        line.write_ansi(0..line.text.len(), &mut ansi);
    }
    ansi
}

fn append_header(output: &mut StyledText, cell: &StyledText, header: ContentStyle) {
    for (mut style, text, link) in cell.rich_spans(0..cell.text.len()) {
        style.foreground_color = header.foreground_color;
        style.attributes.extend(header.attributes);
        output.push_link(style, text, link);
    }
}
fn rule(width: usize, ch: char, style: ContentStyle) -> StyledText {
    let mut line = StyledText::default();
    line.push(style, &ch.to_string().repeat(width));
    line
}
fn table_rule(widths: &[usize], ch: char, style: ContentStyle) -> StyledText {
    let mut line = StyledText::default();
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            line.push(ContentStyle::default(), &" ".repeat(COLUMN_GAP));
        }
        line.push(style, &ch.to_string().repeat(width + CELL_PADDING * 2));
    }
    line
}
fn render_row(
    row: &[Cell],
    widths: &[usize],
    alignments: &[Alignment],
    header: Option<ContentStyle>,
) -> Vec<StyledText> {
    let wrapped = row
        .iter()
        .zip(widths)
        .map(|(cell, width)| wrap_cell(cell, *width))
        .collect::<Vec<_>>();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let mut output = Vec::with_capacity(height);
    for index in 0..height {
        let mut line = StyledText::default();
        let last = wrapped
            .iter()
            .rposition(|cell| cell.get(index).is_some_and(|line| !line.text.is_empty()));
        if let Some(last) = last {
            for column in 0..=last {
                line.push(ContentStyle::default(), " ");
                let cell = wrapped[column].get(index);
                let used = cell.map_or(0, |cell| display_width(&cell.text));
                let remaining = widths[column].saturating_sub(used);
                let left = match alignments.get(column).unwrap_or(&Alignment::None) {
                    Alignment::Center => remaining / 2,
                    Alignment::Right => remaining,
                    _ => 0,
                };
                line.push(ContentStyle::default(), &" ".repeat(left));
                if let Some(cell) = cell {
                    if let Some(style) = header {
                        append_header(&mut line, cell, style);
                    } else {
                        line.append(cell);
                    }
                }
                if column < last {
                    line.push(
                        ContentStyle::default(),
                        &" ".repeat(remaining - left + CELL_PADDING + COLUMN_GAP),
                    );
                }
            }
        }
        output.push(line);
    }
    output
}

struct Table;
impl Table {
    fn cell_display_width(cell: &Cell) -> usize {
        cell.width()
    }
    fn longest_token_width(text: &str) -> usize {
        text.split_whitespace()
            .map(display_width)
            .max()
            .unwrap_or(0)
    }
    fn compute_column_widths(
        metrics: &[TableColumnMetrics],
        available_width: Option<usize>,
    ) -> Option<Vec<usize>> {
        let min_column_width = 3usize;
        let mut widths: Vec<usize> = metrics
            .iter()
            .map(|col| col.max_width.max(min_column_width))
            .collect();

        let Some(max_width) = available_width else {
            return Some(widths);
        };
        let minimum_total = metrics.len() * min_column_width;
        if max_width < minimum_total {
            return None;
        }

        let mut floors: Vec<usize> = metrics
            .iter()
            .map(|col| Self::preferred_column_floor(col, min_column_width))
            .collect();
        let floor_total: usize = floors.iter().sum();
        if floor_total > max_width {
            let minimums = vec![min_column_width; floors.len()];
            Self::shrink_columns(&mut floors, &minimums, metrics, floor_total - max_width);
        }

        let total_width: usize = widths.iter().sum();
        if total_width > max_width {
            let remaining =
                Self::shrink_columns(&mut widths, &floors, metrics, total_width - max_width);
            if remaining > 0 {
                return None;
            }
        }

        Some(widths)
    }
    fn collect_table_column_metrics(
        header: &[Cell],
        rows: &[Vec<Cell>],
        column_count: usize,
    ) -> Vec<TableColumnMetrics> {
        let mut metrics = Vec::with_capacity(column_count);
        for column in 0..column_count {
            let header_cell = &header[column];
            let header_plain = header_cell.plain_text();
            let header_token_width = Self::longest_token_width(header_plain);
            let mut max_width = Self::cell_display_width(header_cell);
            let mut body_token_width = 0usize;
            let mut body_token_count = 0usize;
            let mut long_body_token_count = 0usize;
            let mut total_words = 0usize;
            let mut total_cells = 0usize;
            let mut total_cell_width = 0usize;

            for row in rows {
                let cell = &row[column];
                max_width = max_width.max(Self::cell_display_width(cell));
                let plain = cell.plain_text();
                let mut word_count = 0usize;
                for token in plain.split_whitespace() {
                    let token_width = display_width(token);
                    body_token_width = body_token_width.max(token_width);
                    long_body_token_count += usize::from(token_width >= 20);
                    word_count += 1;
                }
                if word_count > 0 {
                    body_token_count += word_count;
                    total_words += word_count;
                    total_cells += 1;
                    total_cell_width += display_width(plain);
                }
            }

            let narrative = if total_cells == 0 {
                header_plain.split_whitespace().count() >= 4 || display_width(header_plain) >= 28
            } else {
                total_words >= total_cells.saturating_mul(4)
                    || total_cell_width >= total_cells.saturating_mul(28)
            };
            let kind = if long_body_token_count > 0
                && long_body_token_count >= body_token_count.saturating_sub(long_body_token_count)
            {
                TableColumnKind::TokenHeavy
            } else if narrative {
                TableColumnKind::Narrative
            } else {
                TableColumnKind::Compact
            };

            metrics.push(TableColumnMetrics {
                max_width,
                header_token_width,
                body_token_width,
                kind,
            });
        }

        metrics
    }
    fn preferred_column_floor(metrics: &TableColumnMetrics, min_column_width: usize) -> usize {
        let token_target = match metrics.kind {
            TableColumnKind::Narrative | TableColumnKind::TokenHeavy => 16,
            TableColumnKind::Compact => metrics
                .header_token_width
                .max(metrics.body_token_width.min(16)),
        };
        token_target.max(min_column_width).min(metrics.max_width)
    }
    fn shrink_columns(
        widths: &mut [usize],
        floors: &[usize],
        metrics: &[TableColumnMetrics],
        mut amount: usize,
    ) -> usize {
        for kind in [
            TableColumnKind::TokenHeavy,
            TableColumnKind::Narrative,
            TableColumnKind::Compact,
        ] {
            let slack_total = widths
                .iter()
                .enumerate()
                .filter(|(idx, _)| metrics[*idx].kind == kind)
                .map(|(idx, width)| width.saturating_sub(floors[idx]))
                .sum::<usize>();
            let to_remove = amount.min(slack_total);
            if to_remove == 0 {
                continue;
            }

            let mut low = 0usize;
            let mut high = widths
                .iter()
                .enumerate()
                .filter(|(idx, _)| metrics[*idx].kind == kind)
                .map(|(idx, width)| width.saturating_sub(floors[idx]))
                .max()
                .unwrap_or(/*default*/ 0);
            while low < high {
                let cap = low + (high - low) / 2;
                let removed = widths
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| metrics[*idx].kind == kind)
                    .map(|(idx, width)| width.saturating_sub(floors[idx]).saturating_sub(cap))
                    .sum::<usize>();
                if removed > to_remove {
                    low = cap + 1;
                } else {
                    high = cap;
                }
            }

            let cap = low;
            let mut removed = 0usize;
            for (idx, width) in widths.iter_mut().enumerate() {
                if metrics[idx].kind != kind {
                    continue;
                }
                let reduction = width.saturating_sub(floors[idx]).saturating_sub(cap);
                *width -= reduction;
                removed += reduction;
            }

            let mut remainder = to_remove - removed;
            for (idx, width) in widths.iter_mut().enumerate() {
                if remainder == 0 {
                    break;
                }
                if metrics[idx].kind == kind && width.saturating_sub(floors[idx]) == cap {
                    *width -= 1;
                    remainder -= 1;
                }
            }

            amount -= to_remove;
            if amount == 0 {
                break;
            }
        }

        amount
    }
}
fn should_render_records(
    rows: &[Vec<Cell>],
    column_widths: &[usize],
    metrics: &[TableColumnMetrics],
) -> bool {
    if rows.is_empty() {
        return false;
    }

    let affected_rows = rows
        .iter()
        .filter(|row| {
            let contains_fragmented_value =
                row.iter()
                    .zip(column_widths)
                    .zip(metrics)
                    .any(|((cell, width), metrics)| {
                        let has_fragmented_token = cell
                            .plain_text()
                            .split_whitespace()
                            .any(|token| display_width(token) > *width);
                        match metrics.kind {
                            TableColumnKind::Compact => has_fragmented_token,
                            TableColumnKind::TokenHeavy => {
                                *width < MIN_SCANNABLE_TOKEN_HEAVY_WIDTH && has_fragmented_token
                            }
                            TableColumnKind::Narrative => false,
                        }
                    });

            contains_fragmented_value || expansive_cells_are_starved(row, column_widths, metrics)
        })
        .count();
    let threshold = if rows.len() == 1 {
        1
    } else {
        2.max(rows.len().div_ceil(3))
    };

    affected_rows >= threshold
}
fn expansive_cells_are_starved(
    row: &[Cell],
    column_widths: &[usize],
    metrics: &[TableColumnMetrics],
) -> bool {
    let expansive_cells: Vec<(TableColumnKind, usize, usize)> = row
        .iter()
        .zip(column_widths)
        .zip(metrics)
        .filter(|&((_cell, _width), metrics)| metrics.kind != TableColumnKind::Compact)
        .map(|((cell, width), metrics)| (metrics.kind, *width, wrap_cell(cell, *width).len()))
        .collect();

    expansive_cells
        .iter()
        .filter(|(_, _, height)| *height >= CRAMPED_EXPANSIVE_CELL_LINES)
        .count()
        >= 2
        || expansive_cells.iter().any(|(kind, width, height)| {
            *kind == TableColumnKind::Narrative
                && *width < MIN_SCANNABLE_NARRATIVE_WIDTH
                && *height >= CATASTROPHIC_NARRATIVE_CELL_LINES
        })
}

use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crossterm::cursor::{MoveToColumn, RestorePosition, SavePosition};
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor, Stylize};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SyntectStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::{as_24_bit_terminal_escaped, LinesWithEndings};

/// Terminal color capability tier, detected from environment variables.
///
/// `syntect` emits 24-bit truecolor escapes unconditionally; on terminals that
/// only advertise 256-color or 16-color these can render as garbage or as the
/// wrong colors. Detect once at renderer construction and route the highlight
/// path accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSupport {
    /// No ANSI color (`NO_COLOR` set, `TERM=dumb`, or `TERM` unset).
    NoColor,
    /// 4-bit ANSI (8 named colors + 8 bright).
    Ansi16,
    /// 8-bit (256-color palette).
    Ansi256,
    /// 24-bit RGB.
    TrueColor,
}

impl ColorSupport {
    /// Detect the color tier from the process environment.
    ///
    /// Detection precedence:
    /// 1. `NO_COLOR` set to any non-empty value → `NoColor` (https://no-color.org).
    /// 2. `TERM=dumb` or unset → `NoColor`.
    /// 3. `COLORTERM=truecolor` / `24bit`, or `TERM` ending in `-direct` → `TrueColor`.
    /// 4. `TERM` ending in `-256color` → `Ansi256`.
    /// 5. Default → `Ansi256` (assumed by virtually every modern emulator even
    ///    when `TERM=xterm` lacks the suffix; emitting 256-color escapes on a
    ///    16-color terminal degrades gracefully whereas truecolor does not).
    #[must_use]
    pub fn detect() -> Self {
        Self::detect_from_env(|key| std::env::var(key).ok())
    }

    fn detect_from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        if get("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Self::NoColor;
        }
        let term = get("TERM").unwrap_or_default();
        if term.is_empty() || term == "dumb" {
            return Self::NoColor;
        }
        let colorterm = get("COLORTERM").unwrap_or_default();
        if matches!(colorterm.as_str(), "truecolor" | "24bit") || term.ends_with("-direct") {
            return Self::TrueColor;
        }
        if term.ends_with("-256color") {
            return Self::Ansi256;
        }
        Self::Ansi256
    }
}

/// Approximate an RGB triple to the nearest 256-color palette index.
///
/// Uses the standard 6×6×6 RGB cube (indices 16–231) plus the 24-step
/// grayscale ramp (232–255) for near-gray inputs.
fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        let idx: u16 = 232 + (u16::from(r) - 8) / 10;
        return u8::try_from(idx.min(255)).expect("idx clamped to <=255");
    }
    let to_cube = |v: u8| -> u16 { u16::from(v) * 5 / 255 };
    let r5 = to_cube(r);
    let g5 = to_cube(g);
    let b5 = to_cube(b);
    u8::try_from(16 + 36 * r5 + 6 * g5 + b5).expect("cube index fits in u8")
}

/// Render syntect ranges as 256-color escape sequences.
fn ranges_to_256_color_escaped(ranges: &[(SyntectStyle, &str)]) -> String {
    let mut out = String::new();
    for (style, text) in ranges {
        let fg = rgb_to_ansi256(style.foreground.r, style.foreground.g, style.foreground.b);
        let _ = write!(out, "\u{1b}[38;5;{fg}m{text}\u{1b}[0m");
    }
    out
}

/// Strip styling from syntect ranges and concatenate the text.
fn ranges_to_plain(ranges: &[(SyntectStyle, &str)]) -> String {
    let mut out = String::with_capacity(ranges.iter().map(|(_, s)| s.len()).sum());
    for (_, text) in ranges {
        out.push_str(text);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorTheme {
    heading: Color,
    emphasis: Color,
    strong: Color,
    inline_code: Color,
    link: Color,
    quote: Color,
    table_border: Color,
    code_block_border: Color,
    spinner_active: Color,
    spinner_done: Color,
    spinner_failed: Color,
}

impl Default for ColorTheme {
    fn default() -> Self {
        Self {
            heading: Color::Cyan,
            emphasis: Color::Magenta,
            strong: Color::Yellow,
            inline_code: Color::Green,
            link: Color::Blue,
            quote: Color::DarkGrey,
            table_border: Color::DarkCyan,
            code_block_border: Color::DarkGrey,
            spinner_active: Color::Blue,
            spinner_done: Color::Green,
            spinner_failed: Color::Red,
        }
    }
}

pub struct Spinner {
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    thinking: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    const FRAMES_DEFAULT: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    /// Half-circle rotation; visually distinct from braille and recognizable
    /// as "deliberation".
    const FRAMES_THINKING: [&str; 4] = ["◐", "◓", "◑", "◒"];
    const LABEL_THINKING: &'static str = "🧠 Reasoning...";

    #[must_use]
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            pause: Arc::new(AtomicBool::new(false)),
            thinking: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Returns a shared pause flag. Set to `true` before writing content to
    /// prevent the spinner from overwriting output lines. Set back to `false`
    /// after writing to let the spinner resume on the next empty line.
    #[must_use]
    pub fn pause_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.pause)
    }

    /// Returns a shared thinking flag. While set to `true` the spinner
    /// displays a distinct frame set and "Reasoning..." label, signalling
    /// that the model is in an extended-thinking phase rather than emitting
    /// regular content.
    #[must_use]
    pub fn thinking_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.thinking)
    }

    /// Start the spinner animation in a background thread.
    pub fn start(&mut self, label: &str, model: Option<&str>, theme: &ColorTheme) {
        self.stop.store(false, Ordering::SeqCst);
        self.pause.store(false, Ordering::SeqCst);
        self.thinking.store(false, Ordering::SeqCst);
        let stop = Arc::clone(&self.stop);
        let pause = Arc::clone(&self.pause);
        let thinking = Arc::clone(&self.thinking);
        let default_label = label.to_string();
        let model = model.map(ToString::to_string);
        let theme = *theme;
        let start_time = Instant::now();

        self.handle = Some(std::thread::spawn(move || {
            let mut frame_index: usize = 0;
            let mut stdout = io::stdout();
            while !stop.load(Ordering::SeqCst) {
                if !pause.load(Ordering::SeqCst) {
                    let (frames, label): (&[&str], &str) = if thinking.load(Ordering::SeqCst) {
                        (&Self::FRAMES_THINKING[..], Self::LABEL_THINKING)
                    } else {
                        (&Self::FRAMES_DEFAULT[..], default_label.as_str())
                    };
                    let frame = frames[frame_index % frames.len()];
                    frame_index += 1;
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let mut line = format!("{frame} {label}");
                    if let Some(ref m) = model {
                        let _ = write!(line, " [{m}]");
                    }
                    let _ = write!(line, " ({elapsed:.1}s)");
                    let _ = queue!(
                        stdout,
                        SavePosition,
                        MoveToColumn(0),
                        Clear(ClearType::CurrentLine),
                        SetForegroundColor(theme.spinner_active),
                        Print(line),
                        ResetColor,
                        RestorePosition
                    );
                    let _ = stdout.flush();
                }
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        }));
    }

    fn stop_thread(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Stop the spinner and clear its line without printing a final message.
    pub fn clear(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.stop_thread();
        execute!(out, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        out.flush()
    }

    pub fn finish(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.stop_thread();
        execute!(
            out,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_done),
            Print(format!("✔ {label}\n")),
            ResetColor
        )?;
        out.flush()
    }

    pub fn fail(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.stop_thread();
        execute!(
            out,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_failed),
            Print(format!("✘ {label}\n")),
            ResetColor
        )?;
        out.flush()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ListKind {
    Unordered,
    Ordered { next_index: u64 },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TableState {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_head: bool,
}

impl TableState {
    fn push_cell(&mut self) {
        let cell = self.current_cell.trim().to_string();
        self.current_row.push(cell);
        self.current_cell.clear();
    }

    fn finish_row(&mut self) {
        if self.current_row.is_empty() {
            return;
        }
        let row = std::mem::take(&mut self.current_row);
        if self.in_head {
            self.headers = row;
        } else {
            self.rows.push(row);
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RenderState {
    emphasis: usize,
    strong: usize,
    heading_level: Option<u8>,
    quote: usize,
    list_stack: Vec<ListKind>,
    link_stack: Vec<LinkState>,
    table: Option<TableState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkState {
    destination: String,
    text: String,
}

impl RenderState {
    fn style_text(&self, text: &str, theme: &ColorTheme) -> String {
        let mut style = text.stylize();

        if matches!(self.heading_level, Some(1 | 2)) || self.strong > 0 {
            style = style.bold();
        }
        if self.emphasis > 0 {
            style = style.italic();
        }

        if let Some(level) = self.heading_level {
            style = match level {
                1 => style.with(theme.heading),
                2 => style.white(),
                3 => style.with(Color::Blue),
                _ => style.with(Color::Grey),
            };
        } else if self.strong > 0 {
            style = style.with(theme.strong);
        } else if self.emphasis > 0 {
            style = style.with(theme.emphasis);
        }

        if self.quote > 0 {
            style = style.with(theme.quote);
        }

        format!("{style}")
    }

    fn append_raw(&mut self, output: &mut String, text: &str) {
        if let Some(link) = self.link_stack.last_mut() {
            link.text.push_str(text);
        } else if let Some(table) = self.table.as_mut() {
            table.current_cell.push_str(text);
        } else {
            output.push_str(text);
        }
    }

    fn append_styled(&mut self, output: &mut String, text: &str, theme: &ColorTheme) {
        let styled = self.style_text(text, theme);
        self.append_raw(output, &styled);
    }
}

#[derive(Debug)]
pub struct TerminalRenderer {
    syntax_set: SyntaxSet,
    syntax_theme: Theme,
    color_theme: ColorTheme,
    color_support: ColorSupport,
}

impl Default for TerminalRenderer {
    fn default() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let syntax_theme = ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .unwrap_or_default();
        Self {
            syntax_set,
            syntax_theme,
            color_theme: ColorTheme::default(),
            color_support: ColorSupport::detect(),
        }
    }
}

impl TerminalRenderer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn color_theme(&self) -> &ColorTheme {
        &self.color_theme
    }

    #[must_use]
    pub fn color_support(&self) -> ColorSupport {
        self.color_support
    }

    #[cfg(test)]
    pub(crate) fn with_color_support(mut self, support: ColorSupport) -> Self {
        self.color_support = support;
        self
    }

    #[must_use]
    pub fn render_markdown(&self, markdown: &str) -> String {
        let normalized = normalize_nested_fences(markdown);
        let mut output = String::new();
        let mut state = RenderState::default();
        let mut code_language = String::new();
        let mut code_buffer = String::new();
        let mut in_code_block = false;

        for event in Parser::new_ext(&normalized, Options::all()) {
            self.render_event(
                event,
                &mut state,
                &mut output,
                &mut code_buffer,
                &mut code_language,
                &mut in_code_block,
            );
        }

        output
    }

    #[must_use]
    pub fn markdown_to_ansi(&self, markdown: &str) -> String {
        self.render_markdown(markdown)
    }

    #[allow(clippy::too_many_lines)]
    fn render_event(
        &self,
        event: Event<'_>,
        state: &mut RenderState,
        output: &mut String,
        code_buffer: &mut String,
        code_language: &mut String,
        in_code_block: &mut bool,
    ) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                Self::start_heading(state, level as u8, output);
            }
            Event::End(TagEnd::Paragraph) => {
                if state.list_stack.is_empty() {
                    output.push_str("\n\n");
                }
                // Inside a list, End(Item) handles the newline.
            }
            Event::Start(Tag::BlockQuote(..)) => self.start_quote(state, output),
            Event::End(TagEnd::BlockQuote(..)) => {
                state.quote = state.quote.saturating_sub(1);
                output.push('\n');
            }
            Event::End(TagEnd::Heading(..)) => {
                state.heading_level = None;
                output.push_str("\n\n");
            }
            Event::End(TagEnd::Item) | Event::SoftBreak | Event::HardBreak => {
                state.append_raw(output, "\n");
            }
            Event::Start(Tag::List(first_item)) => {
                let kind = match first_item {
                    Some(index) => ListKind::Ordered { next_index: index },
                    None => ListKind::Unordered,
                };
                state.list_stack.push(kind);
            }
            Event::End(TagEnd::List(..)) => {
                state.list_stack.pop();
                output.push('\n');
            }
            Event::Start(Tag::Item) => Self::start_item(state, output),
            Event::Start(Tag::CodeBlock(kind)) => {
                *in_code_block = true;
                *code_language = match kind {
                    CodeBlockKind::Indented => String::from("text"),
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                };
                code_buffer.clear();
                self.start_code_block(code_language, output);
            }
            Event::End(TagEnd::CodeBlock) => {
                self.finish_code_block(code_buffer, code_language, output);
                *in_code_block = false;
                code_language.clear();
                code_buffer.clear();
            }
            Event::Start(Tag::Emphasis) => state.emphasis += 1,
            Event::End(TagEnd::Emphasis) => state.emphasis = state.emphasis.saturating_sub(1),
            Event::Start(Tag::Strong) => state.strong += 1,
            Event::End(TagEnd::Strong) => state.strong = state.strong.saturating_sub(1),
            Event::Code(code) => {
                let rendered =
                    format!("{}", format!("`{code}`").with(self.color_theme.inline_code));
                state.append_raw(output, &rendered);
            }
            Event::Rule => output.push_str("---\n"),
            Event::Text(text) => {
                self.push_text(text.as_ref(), state, output, code_buffer, *in_code_block);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                state.append_raw(output, &html);
            }
            Event::FootnoteReference(reference) => {
                state.append_raw(output, &format!("[{reference}]"));
            }
            Event::TaskListMarker(done) => {
                state.append_raw(output, if done { "[x] " } else { "[ ] " });
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                state.append_raw(output, &math);
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                state.link_stack.push(LinkState {
                    destination: dest_url.to_string(),
                    text: String::new(),
                });
            }
            Event::End(TagEnd::Link) => {
                if let Some(link) = state.link_stack.pop() {
                    let label = if link.text.is_empty() {
                        link.destination.clone()
                    } else {
                        link.text
                    };
                    let rendered = format!(
                        "{}",
                        format!("[{label}]({})", link.destination)
                            .underlined()
                            .with(self.color_theme.link)
                    );
                    state.append_raw(output, &rendered);
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let rendered = format!(
                    "{}",
                    format!("[image:{dest_url}]").with(self.color_theme.link)
                );
                state.append_raw(output, &rendered);
            }
            Event::Start(Tag::Table(..)) => state.table = Some(TableState::default()),
            Event::End(TagEnd::Table) => {
                if let Some(table) = state.table.take() {
                    output.push_str(&self.render_table(&table));
                    output.push_str("\n\n");
                }
            }
            Event::Start(Tag::TableHead) => {
                if let Some(table) = state.table.as_mut() {
                    table.in_head = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(table) = state.table.as_mut() {
                    table.finish_row();
                    table.in_head = false;
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(table) = state.table.as_mut() {
                    table.current_row.clear();
                    table.current_cell.clear();
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(table) = state.table.as_mut() {
                    table.finish_row();
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(table) = state.table.as_mut() {
                    table.current_cell.clear();
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = state.table.as_mut() {
                    table.push_cell();
                }
            }
            Event::Start(Tag::Paragraph | Tag::MetadataBlock(..) | _)
            | Event::End(TagEnd::Image | TagEnd::MetadataBlock(..) | _) => {}
        }
    }

    fn start_heading(state: &mut RenderState, level: u8, output: &mut String) {
        state.heading_level = Some(level);
        if !output.is_empty() {
            output.push('\n');
        }
    }

    fn start_quote(&self, state: &mut RenderState, output: &mut String) {
        state.quote += 1;
        let _ = write!(output, "{}", "│ ".with(self.color_theme.quote));
    }

    fn start_item(state: &mut RenderState, output: &mut String) {
        let depth = state.list_stack.len().saturating_sub(1);
        output.push_str(&"  ".repeat(depth));

        let marker = match state.list_stack.last_mut() {
            Some(ListKind::Ordered { next_index }) => {
                let value = *next_index;
                *next_index += 1;
                format!("{value}. ")
            }
            _ => String::new(),
        };
        output.push_str(&marker);
    }

    fn start_code_block(&self, code_language: &str, output: &mut String) {
        let label = if code_language.is_empty() {
            "code".to_string()
        } else {
            code_language.to_string()
        };
        let _ = writeln!(
            output,
            "{}",
            format!("╭─ {label}")
                .bold()
                .with(self.color_theme.code_block_border)
        );
    }

    fn finish_code_block(&self, code_buffer: &str, code_language: &str, output: &mut String) {
        output.push_str(&self.highlight_code(code_buffer, code_language));
        let _ = write!(
            output,
            "{}",
            "╰─".bold().with(self.color_theme.code_block_border)
        );
        output.push_str("\n\n");
    }

    fn push_text(
        &self,
        text: &str,
        state: &mut RenderState,
        output: &mut String,
        code_buffer: &mut String,
        in_code_block: bool,
    ) {
        if in_code_block {
            code_buffer.push_str(text);
        } else {
            state.append_styled(output, text, &self.color_theme);
        }
    }

    fn render_table(&self, table: &TableState) -> String {
        let mut rows = Vec::new();
        if !table.headers.is_empty() {
            rows.push(table.headers.clone());
        }
        rows.extend(table.rows.iter().cloned());

        if rows.is_empty() {
            return String::new();
        }

        let column_count = rows.iter().map(Vec::len).max().unwrap_or(0);
        let widths = (0..column_count)
            .map(|column| {
                rows.iter()
                    .filter_map(|row| row.get(column))
                    .map(|cell| visible_width(cell))
                    .max()
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>();

        let border = format!("{}", "│".with(self.color_theme.table_border));
        let separator = widths
            .iter()
            .map(|width| "─".repeat(*width + 2))
            .collect::<Vec<_>>()
            .join(&format!("{}", "┼".with(self.color_theme.table_border)));
        let separator = format!("{border}{separator}{border}");

        let mut output = String::new();
        if !table.headers.is_empty() {
            output.push_str(&self.render_table_row(&table.headers, &widths, true));
            output.push('\n');
            output.push_str(&separator);
            if !table.rows.is_empty() {
                output.push('\n');
            }
        }

        for (index, row) in table.rows.iter().enumerate() {
            output.push_str(&self.render_table_row(row, &widths, false));
            if index + 1 < table.rows.len() {
                output.push('\n');
            }
        }

        output
    }

    fn render_table_row(&self, row: &[String], widths: &[usize], is_header: bool) -> String {
        let border = format!("{}", "│".with(self.color_theme.table_border));
        let mut line = String::new();
        line.push_str(&border);

        for (index, width) in widths.iter().enumerate() {
            let cell = row.get(index).map_or("", String::as_str);
            line.push(' ');
            if is_header {
                let _ = write!(line, "{}", cell.bold().with(self.color_theme.heading));
            } else {
                line.push_str(cell);
            }
            let padding = width.saturating_sub(visible_width(cell));
            line.push_str(&" ".repeat(padding + 1));
            line.push_str(&border);
        }

        line
    }

    #[must_use]
    pub fn highlight_code(&self, code: &str, language: &str) -> String {
        if self.color_support == ColorSupport::NoColor {
            return code.to_string();
        }

        let syntax = self
            .syntax_set
            .find_syntax_by_token(language)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
        let mut syntax_highlighter = HighlightLines::new(syntax, &self.syntax_theme);
        let mut colored_output = String::new();

        for line in LinesWithEndings::from(code) {
            match syntax_highlighter.highlight_line(line, &self.syntax_set) {
                Ok(ranges) => {
                    let escaped = match self.color_support {
                        ColorSupport::TrueColor => as_24_bit_terminal_escaped(&ranges[..], false),
                        ColorSupport::Ansi256 => ranges_to_256_color_escaped(&ranges[..]),
                        ColorSupport::Ansi16 | ColorSupport::NoColor => {
                            ranges_to_plain(&ranges[..])
                        }
                    };
                    colored_output.push_str(&self.apply_code_block_background(&escaped));
                }
                Err(_) => colored_output.push_str(&self.apply_code_block_background(line)),
            }
        }

        colored_output
    }

    fn apply_code_block_background(&self, line: &str) -> String {
        // Background tint relies on 256-color escapes; skip when the terminal
        // can't render them cleanly.
        if matches!(
            self.color_support,
            ColorSupport::NoColor | ColorSupport::Ansi16
        ) {
            return line.to_string();
        }
        apply_code_block_background(line)
    }

    pub fn stream_markdown(&self, markdown: &str, out: &mut impl Write) -> io::Result<()> {
        let rendered_markdown = self.markdown_to_ansi(markdown);
        write!(out, "{rendered_markdown}")?;
        if !rendered_markdown.ends_with('\n') {
            writeln!(out)?;
        }
        out.flush()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MarkdownStreamState {
    pending: String,
}

impl MarkdownStreamState {
    #[must_use]
    pub fn push(&mut self, renderer: &TerminalRenderer, delta: &str) -> Option<String> {
        self.pending.push_str(delta);
        let split = find_stream_safe_boundary(&self.pending)?;
        let ready = self.pending[..split].to_string();
        self.pending.drain(..split);
        Some(renderer.markdown_to_ansi(&ready))
    }

    #[must_use]
    pub fn flush(&mut self, renderer: &TerminalRenderer) -> Option<String> {
        if self.pending.trim().is_empty() {
            self.pending.clear();
            None
        } else {
            let pending = std::mem::take(&mut self.pending);
            Some(renderer.markdown_to_ansi(&pending))
        }
    }
}

fn apply_code_block_background(line: &str) -> String {
    let trimmed = line.trim_end_matches('\n');
    let trailing_newline = if trimmed.len() == line.len() {
        ""
    } else {
        "\n"
    };
    let with_background = trimmed.replace("\u{1b}[0m", "\u{1b}[0;48;5;236m");
    format!("\u{1b}[48;5;236m{with_background}\u{1b}[0m{trailing_newline}")
}

/// Pre-process raw markdown so that fenced code blocks whose body contains
/// fence markers of equal or greater length are wrapped with a longer fence.
///
/// LLMs frequently emit triple-backtick code blocks that contain triple-backtick
/// examples.  `CommonMark` (and pulldown-cmark) treats the inner marker as the
/// closing fence, breaking the render.  This function detects the situation and
/// upgrades the outer fence to use enough backticks (or tildes) that the inner
/// markers become ordinary content.
#[allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::manual_repeat_n,
    clippy::manual_str_repeat
)]
fn normalize_nested_fences(markdown: &str) -> String {
    // A fence line is either "labeled" (has an info string ⇒ always an opener)
    // or "bare" (no info string ⇒ could be opener or closer).
    #[derive(Debug, Clone)]
    struct FenceLine {
        char: char,
        len: usize,
        has_info: bool,
        indent: usize,
    }

    fn parse_fence_line(line: &str) -> Option<FenceLine> {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        let indent = trimmed.chars().take_while(|c| *c == ' ').count();
        if indent > 3 {
            return None;
        }
        let rest = &trimmed[indent..];
        let ch = rest.chars().next()?;
        if ch != '`' && ch != '~' {
            return None;
        }
        let len = rest.chars().take_while(|c| *c == ch).count();
        if len < 3 {
            return None;
        }
        let after = &rest[len..];
        if ch == '`' && after.contains('`') {
            return None;
        }
        let has_info = !after.trim().is_empty();
        Some(FenceLine {
            char: ch,
            len,
            has_info,
            indent,
        })
    }

    let lines: Vec<&str> = markdown.split_inclusive('\n').collect();
    // Handle final line that may lack trailing newline.
    // split_inclusive already keeps the original chunks, including a
    // final chunk without '\n' if the input doesn't end with one.

    // First pass: classify every line.
    let fence_info: Vec<Option<FenceLine>> = lines.iter().map(|l| parse_fence_line(l)).collect();

    // Second pass: pair openers with closers using a stack, recording
    // (opener_idx, closer_idx) pairs plus the max fence length found between
    // them.
    struct StackEntry {
        line_idx: usize,
        fence: FenceLine,
    }

    let mut stack: Vec<StackEntry> = Vec::new();
    // Paired blocks: (opener_line, closer_line, max_inner_fence_len)
    let mut pairs: Vec<(usize, usize, usize)> = Vec::new();

    for (i, fi) in fence_info.iter().enumerate() {
        let Some(fl) = fi else { continue };

        if fl.has_info {
            // Labeled fence ⇒ always an opener.
            stack.push(StackEntry {
                line_idx: i,
                fence: fl.clone(),
            });
        } else {
            // Bare fence ⇒ try to close the top of the stack if compatible.
            let closes_top = stack
                .last()
                .is_some_and(|top| top.fence.char == fl.char && fl.len >= top.fence.len);
            if closes_top {
                let opener = stack.pop().unwrap();
                // Find max fence length of any fence line strictly between
                // opener and closer (these are the nested fences).
                let inner_max = fence_info[opener.line_idx + 1..i]
                    .iter()
                    .filter_map(|fi| fi.as_ref().map(|f| f.len))
                    .max()
                    .unwrap_or(0);
                pairs.push((opener.line_idx, i, inner_max));
            } else {
                // Treat as opener.
                stack.push(StackEntry {
                    line_idx: i,
                    fence: fl.clone(),
                });
            }
        }
    }

    // Determine which lines need rewriting.  A pair needs rewriting when
    // its opener length <= max inner fence length.
    struct Rewrite {
        char: char,
        new_len: usize,
        indent: usize,
    }
    let mut rewrites: std::collections::HashMap<usize, Rewrite> = std::collections::HashMap::new();

    for (opener_idx, closer_idx, inner_max) in &pairs {
        let opener_fl = fence_info[*opener_idx].as_ref().unwrap();
        if opener_fl.len <= *inner_max {
            let new_len = inner_max + 1;
            let info_part = {
                let trimmed = lines[*opener_idx]
                    .trim_end_matches('\n')
                    .trim_end_matches('\r');
                let rest = &trimmed[opener_fl.indent..];
                rest[opener_fl.len..].to_string()
            };
            rewrites.insert(
                *opener_idx,
                Rewrite {
                    char: opener_fl.char,
                    new_len,
                    indent: opener_fl.indent,
                },
            );
            let closer_fl = fence_info[*closer_idx].as_ref().unwrap();
            rewrites.insert(
                *closer_idx,
                Rewrite {
                    char: closer_fl.char,
                    new_len,
                    indent: closer_fl.indent,
                },
            );
            // Store info string only in the opener; closer keeps the trailing
            // portion which is already handled through the original line.
            // Actually, we rebuild both lines from scratch below, including
            // the info string for the opener.
            let _ = info_part; // consumed in rebuild
        }
    }

    if rewrites.is_empty() {
        return markdown.to_string();
    }

    // Rebuild.
    let mut out = String::with_capacity(markdown.len() + rewrites.len() * 4);
    for (i, line) in lines.iter().enumerate() {
        if let Some(rw) = rewrites.get(&i) {
            let fence_str: String = std::iter::repeat(rw.char).take(rw.new_len).collect();
            let indent_str: String = std::iter::repeat(' ').take(rw.indent).collect();
            // Recover the original info string (if any) and trailing newline.
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            let fi = fence_info[i].as_ref().unwrap();
            let info = &trimmed[fi.indent + fi.len..];
            let trailing = &line[trimmed.len()..];
            out.push_str(&indent_str);
            out.push_str(&fence_str);
            out.push_str(info);
            out.push_str(trailing);
        } else {
            out.push_str(line);
        }
    }
    out
}

fn find_stream_safe_boundary(markdown: &str) -> Option<usize> {
    let mut open_fence: Option<FenceMarker> = None;
    let mut last_boundary = None;

    for (offset, line) in markdown.split_inclusive('\n').scan(0usize, |cursor, line| {
        let start = *cursor;
        *cursor += line.len();
        Some((start, line))
    }) {
        let line_without_newline = line.trim_end_matches('\n');
        if let Some(opener) = open_fence {
            if line_closes_fence(line_without_newline, opener) {
                open_fence = None;
                last_boundary = Some(offset + line.len());
            }
            continue;
        }

        if let Some(opener) = parse_fence_opener(line_without_newline) {
            open_fence = Some(opener);
            continue;
        }

        if line_without_newline.trim().is_empty() {
            last_boundary = Some(offset + line.len());
        }
    }

    last_boundary
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FenceMarker {
    character: char,
    length: usize,
}

fn parse_fence_opener(line: &str) -> Option<FenceMarker> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let character = rest.chars().next()?;
    if character != '`' && character != '~' {
        return None;
    }
    let length = rest.chars().take_while(|c| *c == character).count();
    if length < 3 {
        return None;
    }
    let info_string = &rest[length..];
    if character == '`' && info_string.contains('`') {
        return None;
    }
    Some(FenceMarker { character, length })
}

fn line_closes_fence(line: &str, opener: FenceMarker) -> bool {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let length = rest.chars().take_while(|c| *c == opener.character).count();
    if length < opener.length {
        return false;
    }
    rest[length..].chars().all(|c| c == ' ' || c == '\t')
}

fn visible_width(input: &str) -> usize {
    strip_ansi(input).chars().count()
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            output.push(ch);
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn detects_no_color_when_no_color_env_set() {
        assert_eq!(
            ColorSupport::detect_from_env(env(&[("NO_COLOR", "1"), ("TERM", "xterm-256color")])),
            ColorSupport::NoColor
        );
    }

    #[test]
    fn empty_no_color_does_not_disable() {
        // The NO_COLOR convention treats an empty value as "not set".
        assert_eq!(
            ColorSupport::detect_from_env(env(&[("NO_COLOR", ""), ("TERM", "xterm-256color")])),
            ColorSupport::Ansi256
        );
    }

    #[test]
    fn detects_no_color_when_term_dumb_or_unset() {
        assert_eq!(
            ColorSupport::detect_from_env(env(&[("TERM", "dumb")])),
            ColorSupport::NoColor
        );
        assert_eq!(
            ColorSupport::detect_from_env(env(&[])),
            ColorSupport::NoColor
        );
    }

    #[test]
    fn detects_truecolor_from_colorterm() {
        assert_eq!(
            ColorSupport::detect_from_env(env(&[("TERM", "xterm"), ("COLORTERM", "truecolor")])),
            ColorSupport::TrueColor
        );
        assert_eq!(
            ColorSupport::detect_from_env(env(&[("TERM", "xterm"), ("COLORTERM", "24bit")])),
            ColorSupport::TrueColor
        );
    }

    #[test]
    fn detects_truecolor_from_direct_term_suffix() {
        assert_eq!(
            ColorSupport::detect_from_env(env(&[("TERM", "xterm-direct")])),
            ColorSupport::TrueColor
        );
    }

    #[test]
    fn detects_256_from_term_suffix() {
        assert_eq!(
            ColorSupport::detect_from_env(env(&[("TERM", "xterm-256color")])),
            ColorSupport::Ansi256
        );
        assert_eq!(
            ColorSupport::detect_from_env(env(&[("TERM", "screen-256color")])),
            ColorSupport::Ansi256
        );
    }

    #[test]
    fn defaults_to_256_for_unspecified_modern_term() {
        assert_eq!(
            ColorSupport::detect_from_env(env(&[("TERM", "xterm")])),
            ColorSupport::Ansi256
        );
    }

    #[test]
    fn no_color_overrides_truecolor_hint() {
        assert_eq!(
            ColorSupport::detect_from_env(env(&[
                ("NO_COLOR", "1"),
                ("COLORTERM", "truecolor"),
                ("TERM", "xterm-256color"),
            ])),
            ColorSupport::NoColor
        );
    }

    #[test]
    fn rgb_to_ansi256_maps_cube_corners() {
        assert_eq!(rgb_to_ansi256(0, 0, 0), 16); // black grayscale shortcut
                                                 // Pure white maps through the grayscale ramp; clamps to the top.
        assert_eq!(rgb_to_ansi256(255, 255, 255), 255);
        assert_eq!(rgb_to_ansi256(255, 0, 0), 16 + 36 * 5); // pure red
        assert_eq!(rgb_to_ansi256(0, 255, 0), 16 + 6 * 5); // pure green
        assert_eq!(rgb_to_ansi256(0, 0, 255), 16 + 5); // pure blue
    }

    #[test]
    fn rgb_to_ansi256_uses_grayscale_ramp_for_mid_gray() {
        // r==g==b in [9, 248] should land in 232..=255.
        let idx = rgb_to_ansi256(128, 128, 128);
        assert!((232..=255).contains(&idx), "got {idx}");
    }

    #[test]
    fn highlight_code_strips_escapes_when_no_color() {
        let renderer = TerminalRenderer::new().with_color_support(ColorSupport::NoColor);
        let code = "fn main() {}\n";
        let out = renderer.highlight_code(code, "rust");
        assert_eq!(out, code);
    }

    #[test]
    fn highlight_code_emits_only_256_color_when_ansi256() {
        let renderer = TerminalRenderer::new().with_color_support(ColorSupport::Ansi256);
        let out = renderer.highlight_code("fn main() {}\n", "rust");
        // No 24-bit truecolor escape sequence.
        assert!(
            !out.contains("\u{1b}[38;2;"),
            "found truecolor escape in: {out:?}"
        );
        // Does contain 256-color escapes.
        assert!(
            out.contains("\u{1b}[38;5;"),
            "missing 256-color escape in: {out:?}"
        );
    }

    #[test]
    fn highlight_code_emits_truecolor_when_truecolor() {
        let renderer = TerminalRenderer::new().with_color_support(ColorSupport::TrueColor);
        let out = renderer.highlight_code("fn main() {}\n", "rust");
        assert!(
            out.contains("\u{1b}[38;2;"),
            "missing truecolor escape in: {out:?}"
        );
    }

    #[test]
    fn spinner_thinking_flag_round_trips() {
        let spinner = Spinner::new();
        let flag = spinner.thinking_flag();
        assert!(
            !flag.load(Ordering::SeqCst),
            "default thinking flag should be off"
        );
        flag.store(true, Ordering::SeqCst);
        // The handle returned by `thinking_flag()` is shared, so consumers
        // toggle the same atomic the spinner thread reads from.
        assert!(spinner.thinking_flag().load(Ordering::SeqCst));
    }

    #[test]
    fn spinner_thinking_and_pause_flags_are_independent() {
        let spinner = Spinner::new();
        let pause = spinner.pause_flag();
        let thinking = spinner.thinking_flag();
        pause.store(true, Ordering::SeqCst);
        assert!(!thinking.load(Ordering::SeqCst));
        thinking.store(true, Ordering::SeqCst);
        assert!(pause.load(Ordering::SeqCst));
    }
}

//! iocraft-based terminal UI for the REPL.
//!
//! During a turn, `TurnRenderer` manages a dedicated render thread that:
//! - Drains `TurnOutput` messages (text from the runner thread) and prints
//!   them above a fixed-position bottom chrome region.
//! - Redraws the bottom chrome (spinner + separators + permission footer)
//!   at 80ms intervals using cursor manipulation.
//!
//! The chrome is positioned using cursor-save/restore and line-clear
//! sequences. Scrollback text is written ABOVE the chrome: the render
//! thread moves the cursor up, inserts lines, then restores the chrome.
//!
//! This approach avoids iocraft's `render_loop()` (which enters raw mode
//! and blocks on stdin events). Instead, it uses iocraft's `element!().print()`
//! for one-shot rendering of the chrome layout at construction time, and
//! manages cursor state manually on the timer loop.

use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ── TurnOutput ─────────────────────────────────────────────────────────

/// Messages from the runner thread to the render thread.
enum OutputMsg {
    Print(String),
    Println(String),
}

/// Channel-backed output handle for routing terminal output through the
/// render thread during a turn. Clone is cheap. Implements `Write` so it
/// can be used as a drop-in replacement for `io::stdout()`.
#[derive(Clone)]
pub struct TurnOutput {
    tx: SyncSender<OutputMsg>,
}

impl TurnOutput {
    pub fn print(&self, text: &str) {
        let _ = self.tx.send(OutputMsg::Print(text.to_string()));
    }

    pub fn println(&self, text: &str) {
        let _ = self.tx.send(OutputMsg::Println(text.to_string()));
    }
}

impl Write for TurnOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf).to_string();
        let _ = self.tx.send(OutputMsg::Print(text));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ── SpinnerState ───────────────────────────────────────────────────────

/// Shared state for the spinner, read by the render thread and written
/// by the runner thread. All fields are atomic for lock-free cross-thread
/// access.
#[derive(Clone)]
pub struct SpinnerState {
    pub response_bytes: Arc<AtomicU32>,
    pub is_thinking: Arc<AtomicBool>,
    pub is_paused: Arc<AtomicBool>,
    start_time: Instant,
    label: String,
    model: Option<String>,
    token_budget: Option<u32>,
}

impl SpinnerState {
    #[must_use]
    pub fn new(label: &str, model: Option<&str>, token_budget: Option<u32>) -> Self {
        Self {
            response_bytes: Arc::new(AtomicU32::new(0)),
            is_thinking: Arc::new(AtomicBool::new(false)),
            is_paused: Arc::new(AtomicBool::new(false)),
            start_time: Instant::now(),
            label: label.to_string(),
            model: model.map(ToString::to_string),
            token_budget,
        }
    }

    /// Render the current spinner frame as a colored ANSI string.
    fn render_frame(&self, frame_index: usize) -> String {
        if self.is_paused.load(Ordering::SeqCst) {
            return String::new();
        }

        let thinking = self.is_thinking.load(Ordering::SeqCst);
        let frames: &[&str] = if thinking {
            &["◐", "◓", "◑", "◒"]
        } else {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        };
        let current_label = if thinking {
            "🧠 Reasoning..."
        } else {
            &self.label
        };

        let frame = frames[frame_index % frames.len()];
        let elapsed = self.start_time.elapsed().as_secs_f64();

        let mut line = format!("{frame} {current_label}");
        if let Some(ref m) = self.model {
            let _ = write!(line, " [{m}]");
        }
        let _ = write!(line, " ({elapsed:.1}s)");

        let bytes = self.response_bytes.load(Ordering::Relaxed);
        if bytes > 0 && elapsed >= 1.0 {
            let approx_tokens = bytes / 4;
            if let Some(budget) = self.token_budget {
                let pct = (f64::from(approx_tokens) / f64::from(budget) * 100.0).min(100.0);
                let fmt_t = format_compact_tokens(approx_tokens);
                let fmt_b = format_compact_tokens(budget);
                let _ = write!(line, " ↓ {fmt_t} / {fmt_b} ({pct:.0}%)");
                if approx_tokens >= 2000 && elapsed > 5.0 {
                    let rate = f64::from(approx_tokens) / elapsed;
                    let remaining = f64::from(budget.saturating_sub(approx_tokens));
                    let eta_secs = remaining / rate;
                    let _ = if eta_secs >= 60.0 {
                        write!(line, " ~{:.0}m", eta_secs / 60.0)
                    } else {
                        write!(line, " ~{eta_secs:.0}s")
                    };
                }
            } else {
                let _ = if approx_tokens >= 1000 {
                    write!(line, " ↓ {:.1}k tokens", f64::from(approx_tokens) / 1000.0)
                } else {
                    write!(line, " ↓ {approx_tokens} tokens")
                };
            }
        }

        // Stall detection: yellow when no new bytes for 3+ seconds.
        let is_stalled = bytes > 0 && !thinking && elapsed > 3.0;
        let color_code = if is_stalled { "33" } else { "34" };
        format!("\x1b[{color_code}m{line}\x1b[0m")
    }
}

fn format_compact_tokens(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", f64::from(tokens) / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", f64::from(tokens) / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Write to stdout from the render thread. Errors are silently ignored
/// (the spinner is cosmetic — losing a frame is harmless).
fn write_render(data: &[u8]) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(data);
    let _ = stdout.flush();
}

// ── Render thread ──────────────────────────────────────────────────────

/// The render loop: drains the output channel and prints to stdout.
/// The spinner is a single-line overwrite (`\r\x1b[2K` + text) — same
/// approach as indicatif's `ProgressBar`, keeping stdout writes minimal
/// to avoid filling the PTY buffer during long streaming responses.
fn render_loop(output_rx: Receiver<OutputMsg>, spinner: SpinnerState, stop: Arc<AtomicBool>) {
    let mut frame_index: usize = 0;
    let mut spinner_visible = false;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        // Drain pending output messages.
        let mut had_output = false;
        loop {
            match output_rx.try_recv() {
                Ok(msg) => {
                    if !had_output && spinner_visible {
                        write_render(b"\r\x1b[2K");
                        spinner_visible = false;
                    }
                    match msg {
                        OutputMsg::Print(text) => {
                            write_render(text.as_bytes());
                        }
                        OutputMsg::Println(text) => {
                            let mut buf = text.into_bytes();
                            buf.push(b'\n');
                            write_render(&buf);
                        }
                    }
                    had_output = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if spinner_visible {
                        write_render(b"\r\x1b[2K");
                    }
                    return;
                }
            }
        }

        if stop.load(Ordering::SeqCst) {
            break;
        }

        // Update spinner — single line overwrite. When paused
        // (e.g. during permission prompts or tool output), clear the
        // spinner line and don't redraw so external code can write to
        // stdout without interference.
        let spinner_text = spinner.render_frame(frame_index);
        if !spinner_text.is_empty() {
            write_render(format!("\r\x1b[2K{spinner_text}").as_bytes());
            spinner_visible = true;
        } else if spinner_visible {
            write_render(b"\r\x1b[2K");
            spinner_visible = false;
        }

        frame_index = frame_index.wrapping_add(1);
        std::thread::sleep(Duration::from_millis(80));
    }

    if spinner_visible {
        write_render(b"\r\x1b[2K");
    }
}

// ── TurnRenderer ───────────────────────────────────────────────────────

/// Manages a render thread during a turn. Created at the start of
/// `run_turn()`, stopped at the end.
pub struct TurnRenderer {
    output: Option<TurnOutput>,
    spinner: SpinnerState,
    stop: Arc<AtomicBool>,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl TurnRenderer {
    /// Start a new render thread for a turn.
    #[must_use]
    pub fn new(label: &str, model: Option<&str>, token_budget: Option<u32>) -> Self {
        let (output_tx, output_rx) = mpsc::sync_channel::<OutputMsg>(256);
        let stop = Arc::new(AtomicBool::new(false));

        let spinner = SpinnerState::new(label, model, token_budget);
        let spinner_clone = spinner.clone();
        let stop_clone = Arc::clone(&stop);

        let join_handle = std::thread::Builder::new()
            .name("repl-render".into())
            .spawn(move || {
                render_loop(output_rx, spinner_clone, stop_clone);
            })
            .expect("spawn repl-render thread");

        Self {
            output: Some(TurnOutput { tx: output_tx }),
            spinner,
            stop,
            join_handle: Some(join_handle),
        }
    }

    /// Get a cloneable output handle for sending text to the render thread.
    pub fn output(&self) -> Option<TurnOutput> {
        self.output.clone()
    }

    /// Get the shared spinner state for the runner to update.
    #[must_use]
    pub fn spinner(&self) -> &SpinnerState {
        &self.spinner
    }

    fn stop_render(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Drop the output sender so the render thread sees Disconnected.
        self.output.take();
        if let Some(h) = self.join_handle.take() {
            // The render thread checks `stop` each 80ms tick and should
            // exit promptly. Use a timeout to avoid blocking forever if
            // the thread is stuck on a stdout write (PTY buffer full).
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            while !h.is_finished() {
                if std::time::Instant::now() >= deadline {
                    // Render thread stuck — abandon it (it will be cleaned
                    // up when the process exits).
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }

    /// Signal the render thread to stop and print a success message.
    pub fn finish(&mut self, label: &str) {
        self.stop_render();
        println!("\x1b[32m✔ {label}\x1b[0m");
    }

    /// Signal the render thread to stop and print a failure message.
    pub fn fail(&mut self, label: &str) {
        self.stop_render();
        println!("\x1b[31m✘ {label}\x1b[0m");
    }

    /// Stop without printing a final message.
    pub fn clear(&mut self) {
        self.stop_render();
    }
}

impl Drop for TurnRenderer {
    fn drop(&mut self) {
        self.stop_render();
    }
}

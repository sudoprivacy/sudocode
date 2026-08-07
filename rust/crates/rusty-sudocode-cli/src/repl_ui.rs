//! iocraft-based terminal UI for the REPL.
//!
//! During a turn, `TurnRenderer` starts an iocraft render loop on a
//! dedicated thread. The render loop manages:
//! - **SpinnerLine**: animated spinner with model/timing/token info
//! - **Chrome**: separator lines + permission mode footer
//!
//! All terminal output during a turn flows through `TurnOutput` (a
//! channel-backed `Write` impl) → iocraft `UseOutput::println()`, which
//! inserts text above the fixed chrome region. This eliminates the
//! cursor-management conflicts between raw ANSI writes and managed UI.

use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iocraft::prelude::*;

// ── TurnOutput ─────────────────────────────────────────────────────────

/// Messages from the runner thread to the iocraft render loop.
enum OutputMsg {
    /// Write text without trailing newline.
    Print(String),
    /// Write text with trailing newline.
    Println(String),
}

/// Channel-backed output handle for routing terminal output through
/// iocraft's `UseOutput` during a turn. Clone is cheap (SyncSender is
/// Arc-wrapped internally). Implements `Write` so it can be used as a
/// drop-in replacement for `io::stdout()`.
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

/// Shared state for the spinner, read by the iocraft component and
/// written by the runner thread. All fields are atomic for lock-free
/// cross-thread access.
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

    /// Render the current spinner frame as a styled string.
    fn render_frame(&self, frame_index: usize) -> String {
        let paused = self.is_paused.load(Ordering::SeqCst);
        if paused {
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

        line
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

// ── Render context ─────────────────────────────────────────────────────

/// Context provided to the iocraft component tree via `ContextProvider`.
struct RenderContext {
    output_rx: Arc<std::sync::Mutex<mpsc::Receiver<OutputMsg>>>,
    spinner: SpinnerState,
    permission_mode: String,
}

// ── iocraft components ─────────────────────────────────────────────────

#[component]
fn TurnChrome(mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let ctx = hooks.use_context::<RenderContext>();
    let spinner = ctx.spinner.clone();
    let permission_mode = ctx.permission_mode.clone();
    let output_rx_mutex = &ctx.output_rx;

    let (stdout, _stderr) = hooks.use_output();
    let mut system = hooks.use_context_mut::<SystemContext>();
    let mut frame_index = hooks.use_state(|| 0usize);
    let mut spinner_text = hooks.use_state(String::new);
    let mut should_exit = hooks.use_state(|| false);

    // Drain the output channel and print above the chrome.
    // Also update spinner frame.
    {
        let spinner_clone = spinner.clone();
        let output_rx = Arc::clone(output_rx_mutex);
        hooks.use_future(async move {
            loop {
                // Drain all pending output messages.
                if let Ok(rx) = output_rx.lock() {
                    loop {
                        match rx.try_recv() {
                            Ok(OutputMsg::Print(text)) => stdout.print(&text),
                            Ok(OutputMsg::Println(text)) => stdout.println(&text),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                should_exit.set(true);
                                return;
                            }
                        }
                    }
                }

                // Update spinner.
                let idx = frame_index.get();
                let text = spinner_clone.render_frame(idx);
                spinner_text.set(text);
                frame_index.set(idx.wrapping_add(1));

                // 80ms tick — matches the original spinner update interval.
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        });
    }

    if *should_exit.read() {
        system.exit();
    }

    let w = crossterm::terminal::size().map_or(80, |(cols, _)| cols as usize);
    let sep = format!("\x1b[2m{}\x1b[0m", "─".repeat(w));

    let icon = match permission_mode.as_str() {
        "danger-full-access" => "⏵⏵",
        "workspace-write" => "⏵",
        _ => "▷",
    };
    let label = match permission_mode.as_str() {
        "danger-full-access" => "full access",
        "workspace-write" => "workspace write",
        "read-only" => "read only",
        other => other,
    };
    let footer = format!(
        "  \x1b[2m{icon} {label} mode · /help for commands · /permissions to change\x1b[0m"
    );

    let spinner_content = spinner_text.read().clone();
    let spinner_color = if spinner_content.is_empty() {
        ""
    } else {
        "\x1b[34m"
    };
    let colored_spinner = if spinner_content.is_empty() {
        String::new()
    } else {
        format!("{spinner_color}{spinner_content}\x1b[0m")
    };

    element! {
        View(flex_direction: FlexDirection::Column) {
            #(if !colored_spinner.is_empty() {
                Some(element! { Text(content: colored_spinner) })
            } else {
                None
            })
            Text(content: sep.clone())
            Text(content: "")
            Text(content: sep)
            Text(content: footer)
        }
    }
}

// ── TurnRenderer ───────────────────────────────────────────────────────

/// Manages an iocraft render loop on a dedicated thread during a turn.
/// Created at the start of `run_turn()`, stopped at the end.
pub struct TurnRenderer {
    output: Option<TurnOutput>,
    spinner: SpinnerState,
    stop_tx: Option<SyncSender<()>>,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl TurnRenderer {
    /// Start a new render loop for a turn.
    #[must_use]
    pub fn new(
        label: &str,
        model: Option<&str>,
        token_budget: Option<u32>,
        permission_mode: &str,
    ) -> Self {
        let (output_tx, output_rx) = mpsc::sync_channel::<OutputMsg>(256);
        let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(1);

        let spinner = SpinnerState::new(label, model, token_budget);
        let spinner_clone = spinner.clone();
        let perm = permission_mode.to_string();

        let join_handle = std::thread::Builder::new()
            .name("repl-render".into())
            .spawn(move || {
                let ctx = RenderContext {
                    output_rx: Arc::new(std::sync::Mutex::new(output_rx)),
                    spinner: spinner_clone,
                    permission_mode: perm,
                };

                // Build a minimal tokio runtime for the timer inside
                // the component's use_future.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .expect("tokio current-thread runtime for render loop");

                rt.block_on(async {
                    let _ = element! {
                        ContextProvider(value: Context::owned(ctx)) {
                            TurnChrome
                        }
                    }
                    .render_loop()
                    .await;
                });

                // Consume the stop signal if it arrived (non-blocking).
                let _ = stop_rx.try_recv();
            })
            .expect("spawn repl-render thread");

        Self {
            output: Some(TurnOutput { tx: output_tx }),
            spinner,
            stop_tx: Some(stop_tx),
            join_handle: Some(join_handle),
        }
    }

    /// Get a cloneable output handle for sending text to the render loop.
    pub fn output(&self) -> Option<TurnOutput> {
        self.output.clone()
    }

    /// Get the shared spinner state for the runner to update.
    #[must_use]
    pub fn spinner(&self) -> &SpinnerState {
        &self.spinner
    }

    fn stop_render(&mut self) {
        // Drop the output sender so the render loop sees Disconnected.
        self.output.take();
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }

    /// Signal the render loop to stop and print a success message.
    pub fn finish(&mut self, label: &str) {
        self.stop_render();
        println!("\x1b[32m✔ {label}\x1b[0m");
    }

    /// Signal the render loop to stop and print a failure message.
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

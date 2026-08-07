//! iocraft-based terminal UI for the REPL.
//!
//! Two layers:
//!
//! 1. **`TurnRenderer`** — per-turn render thread that manages a spinner +
//!    output channel for routing `println!`-style text from the runner thread
//!    to stdout without racing the spinner line.  Used by `LiveCli::run_turn`.
//!
//! 2. **`ReplApp` / `spawn_repl_ui`** — full iocraft REPL component that
//!    replaces rustyline for interactive input.  Owns stdin+stdout via
//!    `render_loop()`, shows a persistent prompt with spinner overlay.
//!    The coordinator thread reads `InputEvent`s from the returned
//!    `ReplHandle` and dispatches turns.

use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iocraft::prelude::*;

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
    active: Arc<AtomicBool>,
    start_time: Arc<Mutex<Instant>>,
    label: Arc<Mutex<String>>,
    model: Arc<Mutex<Option<String>>>,
    token_budget: Arc<Mutex<Option<u32>>>,
}

impl SpinnerState {
    /// Create a new inactive spinner state.
    #[must_use]
    pub fn new_inactive() -> Self {
        Self {
            response_bytes: Arc::new(AtomicU32::new(0)),
            is_thinking: Arc::new(AtomicBool::new(false)),
            is_paused: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(false)),
            start_time: Arc::new(Mutex::new(Instant::now())),
            label: Arc::new(Mutex::new(String::new())),
            model: Arc::new(Mutex::new(None)),
            token_budget: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a new active spinner state (used by TurnRenderer).
    #[must_use]
    pub fn new(label: &str, model: Option<&str>, token_budget: Option<u32>) -> Self {
        Self {
            response_bytes: Arc::new(AtomicU32::new(0)),
            is_thinking: Arc::new(AtomicBool::new(false)),
            is_paused: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(true)),
            start_time: Arc::new(Mutex::new(Instant::now())),
            label: Arc::new(Mutex::new(label.to_string())),
            model: Arc::new(Mutex::new(model.map(ToString::to_string))),
            token_budget: Arc::new(Mutex::new(token_budget)),
        }
    }

    /// Reset and activate for a new turn.
    pub fn start_turn(&self, label: &str, model: Option<&str>, token_budget: Option<u32>) {
        self.response_bytes.store(0, Ordering::SeqCst);
        self.is_thinking.store(false, Ordering::SeqCst);
        self.is_paused.store(false, Ordering::SeqCst);
        *self.start_time.lock().unwrap() = Instant::now();
        *self.label.lock().unwrap() = label.to_string();
        *self.model.lock().unwrap() = model.map(ToString::to_string);
        *self.token_budget.lock().unwrap() = token_budget;
        self.active.store(true, Ordering::SeqCst);
    }

    /// Deactivate after a turn ends.
    pub fn stop_turn(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    /// Render the current spinner frame as a colored ANSI string.
    /// Returns empty string when inactive or paused.
    pub fn render_frame(&self, frame_index: usize) -> String {
        if !self.active.load(Ordering::SeqCst) {
            return String::new();
        }
        if self.is_paused.load(Ordering::SeqCst) {
            return String::new();
        }

        let thinking = self.is_thinking.load(Ordering::SeqCst);
        let frames: &[&str] = if thinking {
            &["\u{25d0}", "\u{25d3}", "\u{25d1}", "\u{25d2}"]
        } else {
            &[
                "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
                "\u{2827}", "\u{2807}", "\u{280f}",
            ]
        };
        let label = self.label.lock().unwrap();
        let current_label = if thinking {
            "\u{1f9e0} Reasoning..."
        } else {
            &label
        };

        let frame = frames[frame_index % frames.len()];
        let elapsed = self.start_time.lock().unwrap().elapsed().as_secs_f64();

        let mut line = format!("{frame} {current_label}");
        if let Some(ref m) = *self.model.lock().unwrap() {
            let _ = write!(line, " [{m}]");
        }
        let _ = write!(line, " ({elapsed:.1}s)");

        let bytes = self.response_bytes.load(Ordering::Relaxed);
        let token_budget = *self.token_budget.lock().unwrap();
        if bytes > 0 && elapsed >= 1.0 {
            let approx_tokens = bytes / 4;
            if let Some(budget) = token_budget {
                let pct = (f64::from(approx_tokens) / f64::from(budget) * 100.0).min(100.0);
                let fmt_t = format_compact_tokens(approx_tokens);
                let fmt_b = format_compact_tokens(budget);
                let _ = write!(line, " \u{2193} {fmt_t} / {fmt_b} ({pct:.0}%)");
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
                    write!(
                        line,
                        " \u{2193} {:.1}k tokens",
                        f64::from(approx_tokens) / 1000.0
                    )
                } else {
                    write!(line, " \u{2193} {approx_tokens} tokens")
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
/// (the spinner is cosmetic -- losing a frame is harmless).
fn write_render(data: &[u8]) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(data);
    let _ = stdout.flush();
}

// ── Render thread ──────────────────────────────────────────────────────

/// The render loop: drains the output channel and prints to stdout.
/// The spinner is a single-line overwrite (`\r\x1b[2K` + text).
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

        // Update spinner.
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
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            while !h.is_finished() {
                if std::time::Instant::now() >= deadline {
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
        println!("\x1b[32m\u{2714} {label}\x1b[0m");
    }

    /// Signal the render thread to stop and print a failure message.
    pub fn fail(&mut self, label: &str) {
        self.stop_render();
        println!("\x1b[31m\u{2718} {label}\x1b[0m");
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

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// iocraft REPL — replaces rustyline for interactive input
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Channel-backed output handle for routing text from the runner thread
/// to the iocraft render loop. Clone is cheap. Implements `std::io::Write`
/// so it can be used as a stdout replacement.
#[derive(Clone)]
pub struct OutputSender {
    tx: SyncSender<String>,
}

impl OutputSender {
    pub fn println(&self, text: &str) {
        let _ = self.tx.send(text.to_string());
    }
}

impl Write for OutputSender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf).to_string();
        let _ = self.tx.send(text);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Events from the iocraft UI to the coordinator thread.
pub enum InputEvent {
    Submit(String),
    /// ESC pressed — cancel the running turn.
    Abort,
    Exit,
}

/// Handle returned by `spawn_repl_ui` for the coordinator to use.
pub struct ReplHandle {
    pub output: OutputSender,
    pub input_rx: Receiver<InputEvent>,
    pub spinner: SpinnerState,
    ui_thread: Option<std::thread::JoinHandle<()>>,
}

impl ReplHandle {
    /// Wait for the iocraft render loop thread to exit, with a timeout.
    /// If the thread doesn't exit within 500ms (e.g. Windows PTY
    /// interaction), abandon it — the process exit will clean up.
    pub fn join(self) {
        // Drop the output sender so the render loop sees Disconnected.
        let ui_thread = self.ui_thread;
        drop(self.output);
        drop(self.input_rx);
        if let Some(h) = ui_thread {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while !h.is_finished() {
                if std::time::Instant::now() >= deadline {
                    // iocraft render_loop didn't exit in time (Windows PTY).
                    // Force process exit — all persistent state has been
                    // flushed by the coordinator before calling join().
                    std::process::exit(0);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let _ = h.join();
        }
    }
}

/// Context passed to `ReplApp` via `ContextProvider`.
struct ReplContext {
    output_rx: Arc<Mutex<Receiver<String>>>,
    input_tx: SyncSender<InputEvent>,
    spinner: SpinnerState,
    permission_mode: String,
}

#[component]
fn ReplApp(mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let ctx = hooks.use_context::<ReplContext>();
    let output_rx = Arc::clone(&ctx.output_rx);
    let input_tx = ctx.input_tx.clone();
    let spinner = ctx.spinner.clone();
    let permission_mode = ctx.permission_mode.clone();
    drop(ctx);

    let (stdout, _stderr) = hooks.use_output();
    let mut system = hooks.use_context_mut::<SystemContext>();
    let mut input_value = hooks.use_state(String::new);
    let mut frame = hooks.use_state(|| 0usize);
    let mut spinner_text = hooks.use_state(String::new);
    let mut should_exit = hooks.use_state(|| false);
    let mut last_ctrlc = hooks.use_state(|| None::<Instant>);

    // Clone handles for the future (StdoutHandle is Clone).
    let stdout_for_future = stdout.clone();
    let spinner_for_future = spinner.clone();
    let output_rx_for_future = Arc::clone(&output_rx);

    // 80ms tick loop: drain output channel, update spinner text.
    hooks.use_future(async move {
        loop {
            smol::Timer::after(Duration::from_millis(80)).await;

            // Drain output channel.
            if let Ok(rx) = output_rx_for_future.lock() {
                loop {
                    match rx.try_recv() {
                        Ok(text) => stdout_for_future.println(text),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => break,
                    }
                }
            }

            // Update spinner.
            let idx = frame.get();
            frame.set(idx.wrapping_add(1));
            let text = spinner_for_future.render_frame(idx);
            spinner_text.set(text);
        }
    });

    // Clone for the terminal event handler.
    let input_tx_for_events = input_tx.clone();
    let stdout_for_events = stdout.clone();

    hooks.use_terminal_events({
        move |event| match event {
            TerminalEvent::Key(KeyEvent {
                code,
                kind,
                modifiers,
                ..
            }) if kind != KeyEventKind::Release => {
                match code {
                    KeyCode::Enter if !modifiers.contains(KeyModifiers::SHIFT) => {
                        let val = input_value.read().clone();
                        if !val.trim().is_empty() {
                            let trimmed = val.trim();
                            if trimmed == "/exit" || trimmed == "/quit" {
                                let _ = input_tx_for_events.send(InputEvent::Exit);
                                should_exit.set(true);
                            } else {
                                // Echo the input above.
                                stdout_for_events.println(format!("\x1b[1m\u{276f} {val}\x1b[0m"));
                                let _ = input_tx_for_events.send(InputEvent::Submit(val));
                            }
                            input_value.set(String::new());
                        }
                    }
                    KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                        let _ = input_tx_for_events.send(InputEvent::Exit);
                        should_exit.set(true);
                    }
                    KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                        input_value.set(String::new());
                    }
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                        // Ctrl-C: first press shows hint + cancels turn;
                        // second press within 800ms exits.
                        let now = Instant::now();
                        if last_ctrlc
                            .read()
                            .is_some_and(|t| now.duration_since(t).as_millis() < 800)
                        {
                            let _ = input_tx_for_events.send(InputEvent::Exit);
                            should_exit.set(true);
                        } else {
                            last_ctrlc.set(Some(now));
                            let _ = input_tx_for_events.send(InputEvent::Abort);
                            stdout_for_events.println("  \x1b[2mPress Ctrl-C again to exit\x1b[0m");
                            input_value.set(String::new());
                        }
                    }
                    KeyCode::Esc => {
                        let _ = input_tx_for_events.send(InputEvent::Abort);
                        input_value.set(String::new());
                    }
                    _ => {
                        // Let TextInput handle all other keys via on_change.
                    }
                }
            }
            _ => {}
        }
    });

    // Exit check: `system` was obtained before the event handler and is
    // NOT captured by the Send closure. The exit flag is set inside the
    // closure; we check it here outside the closure.
    if *should_exit.read() {
        system.exit();
    }

    let st = spinner_text.read().clone();
    let val = input_value.read().clone();
    let perm = permission_mode.clone();

    element! {
        View(flex_direction: FlexDirection::Column) {
            #(if !st.is_empty() {
                Some(element! { Text(content: st.clone()) })
            } else {
                None
            })
            Text(content: "\u{2500}".repeat(60), color: Color::DarkGrey)
            View(flex_direction: FlexDirection::Row) {
                Text(content: "\u{276f} ")
                TextInput(
                    value: val,
                    has_focus: true,
                    on_change: move |new_val: String| {
                        input_value.set(new_val);
                    },
                )
            }
            Text(content: "\u{2500}".repeat(60), color: Color::DarkGrey)
            Text(
                content: format!("  \u{23f5}\u{23f5} {perm} \u{00b7} /help \u{00b7} /exit to quit"),
                color: Color::DarkGrey,
            )
        }
    }
}

/// Spawn the iocraft REPL UI on a dedicated thread and return a handle
/// for the coordinator to communicate with it.
///
/// The coordinator reads `InputEvent`s from `ReplHandle::input_rx` and
/// sends output text via `ReplHandle::output`. The spinner state is
/// shared so the runner thread can update it atomically.
pub fn spawn_repl_ui(permission_mode: &str, startup_banner: &str) -> ReplHandle {
    let (output_tx, output_rx) = mpsc::sync_channel::<String>(512);
    let (input_tx, input_rx) = mpsc::sync_channel::<InputEvent>(16);
    let spinner = SpinnerState::new_inactive();

    let ctx = ReplContext {
        output_rx: Arc::new(Mutex::new(output_rx)),
        input_tx: input_tx.clone(),
        spinner: spinner.clone(),
        permission_mode: permission_mode.to_string(),
    };

    let banner = startup_banner.to_string();
    let join_handle = std::thread::Builder::new()
        .name("repl-ui".into())
        .spawn(move || {
            // Print banner before entering the render loop so the user sees
            // session info in the scrollback. iocraft's render_loop enters
            // raw mode immediately, so we print first.
            println!("{banner}");
            smol::block_on(
                element! {
                    ContextProvider(value: Context::owned(ctx)) {
                        ReplApp
                    }
                }
                .render_loop()
                .ignore_ctrl_c(),
            )
            .expect("iocraft render_loop failed");
        })
        .expect("spawn repl-ui thread");
    // Small delay to let the render loop enter raw mode before the
    // coordinator starts sending events.
    std::thread::sleep(std::time::Duration::from_millis(50));

    ReplHandle {
        output: OutputSender { tx: output_tx },
        input_rx,
        spinner,
        ui_thread: Some(join_handle),
    }
}

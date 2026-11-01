//! Foreground activity projection. Engine lifecycle owns busy/input/idle;
//! animation and temporary output suspension never change that lifecycle.

use std::{
    fmt::Write,
    sync::{Arc, Mutex},
    time::Instant,
};

use crossterm::style::SetForegroundColor;
use engine_events::EngineState;

use super::{spinner_progress::SpinnerProgress, text_layout, theme, RESET};

mod brand_spinner;
pub(crate) use brand_spinner::frame as spinner_frame;

pub(crate) const WORKING_LABEL: &str = "Working...";
pub(super) const THINKING_LABEL: &str = "Thinking...";

#[derive(Clone)]
enum ActivityPhase {
    Working,
    Reasoning,
    Retry {
        attempt: u32,
        max_retries: u32,
        reason: String,
    },
}

struct TurnMetadata {
    start: Instant,
    model: Option<String>,
    token_budget: Option<u32>,
}

/// Shared renderer projection fed by the session's ordered engine event bridge.
#[derive(Clone)]
pub(crate) struct ActivityState {
    pub(crate) progress: Arc<SpinnerProgress>,
    lifecycle: Arc<Mutex<EngineState>>,
    phase: Arc<Mutex<ActivityPhase>>,
    metadata: Arc<Mutex<TurnMetadata>>,
}

impl ActivityState {
    pub(crate) fn new() -> Self {
        Self {
            progress: Arc::new(SpinnerProgress::new()),
            lifecycle: Arc::new(Mutex::new(EngineState::Idle)),
            phase: Arc::new(Mutex::new(ActivityPhase::Working)),
            metadata: Arc::new(Mutex::new(TurnMetadata {
                start: Instant::now(),
                model: None,
                token_budget: None,
            })),
        }
    }

    /// Prepare counters before submission; only engine events activate a turn.
    pub(crate) fn prepare_turn(&self, model: &str, token_budget: Option<u32>) {
        self.progress.reset();
        *self.phase.lock().unwrap() = ActivityPhase::Working;
        *self.metadata.lock().unwrap() = TurnMetadata {
            start: Instant::now(),
            model: Some(model.to_string()),
            token_budget,
        };
    }

    #[inline]
    pub(crate) fn engine_state(&self) -> EngineState {
        *self.lifecycle.lock().unwrap()
    }

    /// Called only by the event bridge, including while the renderer is idle.
    #[inline]
    pub(crate) fn observe_engine_state(&self, state: EngineState) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        if *lifecycle != state {
            *lifecycle = state;
            self.progress.record_activity();
        }
    }

    pub(crate) fn set_reasoning(&self, on: bool) {
        *self.phase.lock().unwrap() = if on {
            ActivityPhase::Reasoning
        } else {
            ActivityPhase::Working
        };
    }

    pub(crate) fn set_retry(&self, attempt: u32, max_retries: u32, reason: &str) {
        *self.phase.lock().unwrap() = ActivityPhase::Retry {
            attempt,
            max_retries,
            reason: reason.to_string(),
        };
    }

    /// Render one status content variant; lifecycle still owns the footer.
    pub(crate) fn render_frame(
        &self,
        frame_index: usize,
        state: EngineState,
        tools: ToolActivity<'_>,
        width: usize,
    ) -> String {
        // Questions and approvals already own InputSlot. Remove the working
        // row while waiting rather than adding a second instruction above it.
        if state != EngineState::Running {
            return String::new();
        }
        let phase = self.phase.lock().unwrap();
        let content = match &*phase {
            ActivityPhase::Retry {
                attempt,
                max_retries,
                reason,
            } => StatusContent::Retry {
                attempt: *attempt,
                max_retries: *max_retries,
                reason,
            },
            ActivityPhase::Reasoning => StatusContent::Reasoning,
            ActivityPhase::Working if tools.count > 0 || tools.progress.is_some() => {
                StatusContent::Tools(tools)
            }
            ActivityPhase::Working => StatusContent::Working,
        };
        let reasoning = matches!(content, StatusContent::Reasoning);
        let warning =
            matches!(content, StatusContent::Retry { .. }) || self.progress.is_stalled(reasoning);
        let label = content.label();
        let metadata = self.metadata.lock().unwrap();
        progress_line(
            frame_index,
            &label,
            ProgressValues {
                model: metadata.model.as_deref(),
                elapsed: metadata.start.elapsed().as_secs_f64(),
                bytes: self.progress.response_bytes(),
                token_budget: metadata.token_budget,
            },
            width,
            text_layout::ColumnMetric::Canvas,
            warning,
        )
    }
}

/// Borrowed projection of the existing pending cards and latest progress.
/// This is not a second tool registry or independently maintained counter.
#[derive(Default)]
pub(crate) struct ToolActivity<'a> {
    pub(crate) count: usize,
    pub(crate) name: Option<&'a str>,
    pub(crate) progress: Option<&'a str>,
}

/// Mutually exclusive content modules inside the existing status row.
enum StatusContent<'a> {
    Working,
    Reasoning,
    Tools(ToolActivity<'a>),
    Retry {
        attempt: u32,
        max_retries: u32,
        reason: &'a str,
    },
}

impl StatusContent<'_> {
    #[inline]
    fn label(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Working => WORKING_LABEL.into(),
            Self::Reasoning => THINKING_LABEL.into(),
            Self::Retry {
                attempt,
                max_retries,
                reason,
            } => format!("Retry {attempt}/{max_retries}: {reason}").into(),
            Self::Tools(tools) => {
                // Actual progress has priority; pending cards already show the
                // tool identities. Quiet tools fall back to their name/count.
                if let Some(progress) = tools.progress {
                    return progress.into();
                }
                match tools.count {
                    0 => "Running…".to_string(),
                    1 => format!("Running {}…", tools.name.unwrap_or("tool")),
                    count => format!("Running {count} tools…"),
                }
                .into()
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct ProgressValues<'a> {
    pub(super) model: Option<&'a str>,
    pub(super) elapsed: f64,
    pub(super) bytes: u32,
    pub(super) token_budget: Option<u32>,
}

/// One formatter for inline and one-shot model/time/token/budget/ETA display.
#[inline]
fn progress_details(values: ProgressValues<'_>) -> String {
    let ProgressValues {
        model,
        elapsed,
        bytes,
        token_budget,
    } = values;
    // Keep time-derived metadata stable between seconds while the logo keeps
    // animating. Token changes still refresh immediately; turn statistics use
    // the original precise duration outside this presentation formatter.
    let elapsed = elapsed.floor();
    let mut details = String::new();
    if let Some(model) = model {
        let _ = write!(details, " [{model}]");
    }
    let _ = write!(details, " ({elapsed:.0}s)");
    if bytes > 0 && elapsed >= 1.0 {
        let tokens = bytes / 4;
        if let Some(budget) = token_budget {
            let pct = (f64::from(tokens) / f64::from(budget) * 100.0).min(100.0);
            let _ = write!(
                details,
                " ↓ {} / {} ({pct:.0}%)",
                compact_tokens(tokens),
                compact_tokens(budget)
            );
            if tokens >= 2000 && elapsed > 5.0 {
                let rate = f64::from(tokens) / elapsed;
                let eta = f64::from(budget.saturating_sub(tokens)) / rate;
                let _ = if eta >= 60.0 {
                    write!(details, " ~{:.0}m", eta / 60.0)
                } else {
                    write!(details, " ~{eta:.0}s")
                };
            }
        } else if tokens >= 1000 {
            let _ = write!(details, " ↓ {:.1}k tokens", f64::from(tokens) / 1000.0);
        } else {
            let _ = write!(details, " ↓ {tokens} tokens");
        }
    }
    details
}

/// Lay out the activity row using the shared metadata and semantic colors.
#[inline]
pub(super) fn progress_line(
    frame_index: usize,
    label: &str,
    values: ProgressValues<'_>,
    width: usize,
    metric: text_layout::ColumnMetric,
    warning: bool,
) -> String {
    let details = progress_details(values);
    let logo = spinner_frame(frame_index);
    let reserved = metric.width(logo) + 1 + metric.width(&details);
    let label = text_layout::single_line(label);
    let label = metric.truncate(&label, width.saturating_sub(reserved));
    let colors = theme().activity_colors(warning);
    let line = format!(
        "{}{logo}{} {label}{}{details}{RESET}",
        SetForegroundColor(colors.logo),
        SetForegroundColor(colors.label),
        SetForegroundColor(colors.metadata),
    );
    // On extremely narrow terminals even metadata cannot fit. Keep one row.
    if reserved > width {
        metric.truncate(&line, width)
    } else {
        line
    }
}

fn compact_tokens(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", f64::from(tokens) / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", f64::from(tokens) / 1_000.0)
    } else {
        tokens.to_string()
    }
}

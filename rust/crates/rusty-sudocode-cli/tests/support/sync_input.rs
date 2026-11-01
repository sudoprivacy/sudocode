use std::time::{Duration, Instant};

use pty_expect::PtySession;

use super::common;

/// Wait for the synchronous editor's new prompt after this completed turn.
///
/// An empty prompt retained above the result is not readiness: completion
/// discovery can still be running before the next read. Rustyline prints its
/// new prompt only after enabling raw mode and initializing its reader.
pub fn expect_prompt_after_turn(
    session: &PtySession,
    marker: &str,
    budget: Duration,
    context: &str,
) {
    let deadline = Instant::now() + budget;
    common::expect_turn_complete_after(session, marker, budget, context);
    common::expect_screen_settled(
        session,
        |screen| {
            let (Some(prompt), Some(status)) = (screen.rfind('❯'), screen.rfind("· turn "))
            else {
                return false;
            };
            prompt > status && common::input_line_of(screen).is_empty()
        },
        deadline.saturating_duration_since(Instant::now()),
        context,
    );
}

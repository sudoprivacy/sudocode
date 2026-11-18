//! Opt-in local input observations, without typed characters or pasted content.

use iocraft::prelude::KeyCode;
use std::fs::{File, OpenOptions};
use std::io::Write;

#[derive(Clone, Copy)]
pub(super) enum ObservedInput {
    Key(KeyCode),
    Paste,
}

pub(super) struct InputTrace {
    file: File,
    sequence: u64,
}

impl InputTrace {
    pub(super) fn open() -> Option<Self> {
        let path = std::env::var_os("SCODE_TRACE_INPUT")?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Self { file, sequence: 0 })
    }

    /// Record the existing frame guard's observation before dispatch. This
    /// neither admits input nor changes the event queue or rendered chrome.
    pub(super) fn record(
        &mut self,
        input: ObservedInput,
        size: (u16, u16),
        reviewable: bool,
        frame_current: bool,
    ) {
        let (event, function) = match input {
            ObservedInput::Paste => ("paste", None),
            ObservedInput::Key(KeyCode::F(number)) => ("function", Some(number)),
            ObservedInput::Key(KeyCode::Enter) => ("enter", None),
            ObservedInput::Key(KeyCode::Esc) => ("escape", None),
            ObservedInput::Key(KeyCode::Char(_)) => ("character", None),
            ObservedInput::Key(_) => ("key", None),
        };
        self.sequence += 1;
        let record = serde_json::json!({
            "pid": std::process::id(),
            "sequence": self.sequence,
            "event": event,
            "function": function,
            "width": size.0,
            "height": size.1,
            "reviewable": reviewable,
            "frame_current": frame_current,
        });
        // Diagnostics are best-effort; an unavailable destination must never
        // change input behavior. Each newline terminates one complete record.
        let _ = self.file.write_all(format!("{record}\n").as_bytes());
    }
}

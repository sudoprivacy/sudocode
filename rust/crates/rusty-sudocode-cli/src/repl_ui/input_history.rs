//! History navigation is an editor policy, applied against the event-time
//! value/cursor before the next queued key. It must not depend on a render
//! snapshot or a deferred imperative cursor request from the parent component.

use super::{PendingItem, UpArrowDequeueHook};
use iocraft::prelude::{KeyCode, KeyEvent, KeyModifiers, State, TerminalEvent, TextInputEdit};
use std::sync::{Arc, Mutex};

pub(super) struct InputHistory {
    pub history: State<Vec<String>>,
    pub selected: State<Option<usize>>,
    pub saved_input: State<String>,
    pub dequeue: Option<UpArrowDequeueHook>,
    pub pending: Arc<Mutex<Vec<PendingItem>>>,
}

impl InputHistory {
    pub fn edit(
        &mut self,
        event: &TerminalEvent,
        current: &str,
        cursor: usize,
    ) -> Option<TextInputEdit> {
        let TerminalEvent::Key(KeyEvent {
            code, modifiers, ..
        }) = event
        else {
            return None;
        };
        match code {
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected.set(None);
                Some(at_end(String::new()))
            }
            KeyCode::Up => {
                if let Some(selected) = self.selected.get() {
                    let history = self.history.read();
                    if history.is_empty() {
                        return None;
                    }
                    let previous = selected.saturating_sub(1).min(history.len() - 1);
                    let value = history[previous].clone();
                    self.selected.set(Some(previous));
                    return Some(at_end(value));
                }
                if current.is_empty() {
                    // One Up recalls all human chips; inbound peer chips stay
                    // queued. The coordinator remains the dequeue authority.
                    if let Some(value) = self.dequeue.as_ref().and_then(|hook| hook()) {
                        if let Ok(mut pending) = self.pending.lock() {
                            pending.retain(|item| {
                                !matches!(item, PendingItem::QueuedMessage { is_human: true, .. })
                            });
                        }
                        return Some(at_end(value));
                    }
                }
                // Logical lines are delimited by a real newline, not wrapping.
                // Other lines retain TextInput's normal vertical navigation.
                if current[..cursor].contains('\n') {
                    return None;
                }
                if cursor != 0 {
                    return Some(TextInputEdit {
                        value: current.to_owned(),
                        cursor_offset: 0,
                    });
                }
                let history = self.history.read();
                let last = history.len().checked_sub(1)?;
                let value = history[last].clone();
                self.saved_input.set(current.to_owned());
                self.selected.set(Some(last));
                Some(at_end(value))
            }
            KeyCode::Down => {
                if let Some(selected) = self.selected.get() {
                    let history = self.history.read();
                    if selected + 1 < history.len() {
                        let value = history[selected + 1].clone();
                        self.selected.set(Some(selected + 1));
                        return Some(at_end(value));
                    }
                    self.selected.set(None);
                    return Some(at_end(self.saved_input.read().clone()));
                }
                if cursor < current.len() && !current[cursor..].contains('\n') {
                    return Some(at_end(current.to_owned()));
                }
                None
            }
            _ => None,
        }
    }
}

fn at_end(value: String) -> TextInputEdit {
    TextInputEdit {
        cursor_offset: value.len(),
        value,
    }
}

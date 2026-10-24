//! Exercise shutdown through the public engine seam while a host retains callbacks.
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use engine_core::{EngineDelegate, EngineSession};
use engine_events::{ContentBlock, EngineCommand, EngineEvent, PermissionMode, TurnComplete};
use runtime::{
    HookAbortSignal, PermissionPrompter, QuestionPrompter, RuntimeError, RuntimeObserver,
};

struct RetainedHost {
    question: Mutex<Option<Box<dyn QuestionPrompter>>>,
    close_entered: mpsc::Sender<()>,
    close_release: Mutex<mpsc::Receiver<()>>,
}

impl EngineDelegate for RetainedHost {
    fn run_turn(
        &self,
        _blocks: Vec<ContentBlock>,
        _observer: &mut dyn RuntimeObserver,
        _prompter: &mut dyn PermissionPrompter,
    ) -> Result<TurnComplete, RuntimeError> {
        Ok(TurnComplete {
            iterations: 0,
            turn_usage: runtime::TokenUsage::default(),
            session_usage: runtime::TokenUsage::default(),
            cancelled: false,
            response_model: None,
            auto_compaction: None,
        })
    }

    fn set_question_prompter(&self, prompter: Box<dyn QuestionPrompter>) {
        *self.question.lock().unwrap() = Some(prompter);
    }

    fn abort_signal(&self) -> HookAbortSignal {
        HookAbortSignal::default()
    }

    fn set_model(&self, model: &str) -> Result<(String, Vec<String>), String> {
        Ok((model.to_owned(), Vec::new()))
    }

    fn set_permission_mode(&self, _mode: PermissionMode) -> Result<(), String> {
        Ok(())
    }

    fn handle_slash_command(&self, line: &str) -> Result<String, String> {
        Ok(line.to_owned())
    }

    fn close(&self) {
        self.close_entered.send(()).unwrap();
        self.close_release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }
}

fn prompt() -> EngineCommand {
    EngineCommand::Prompt {
        blocks: vec![ContentBlock::Text {
            text: "turn".into(),
        }],
        context: None,
    }
}

#[test]
fn close_disconnects_events_even_when_host_retains_a_question_callback() {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let host = Arc::new(RetainedHost {
        question: Mutex::new(None),
        close_entered: entered_tx,
        close_release: Mutex::new(release_rx),
    });
    let handle = EngineSession::spawn(host.clone());
    handle.commands.send(prompt()).unwrap();
    loop {
        if matches!(
            handle.events.recv_timeout(Duration::from_secs(5)).unwrap(),
            EngineEvent::TurnComplete(_)
        ) {
            break;
        }
    }
    assert!(host.question.lock().unwrap().is_some());
    handle.commands.send(EngineCommand::Close).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // This command is accepted by the transport after Close, but never admitted
    // by the engine. A renderer waiting for its result must observe disconnect.
    handle.commands.send(prompt()).unwrap();
    release_tx.send(()).unwrap();
    loop {
        match handle.events.recv_timeout(Duration::from_secs(5)) {
            Ok(EngineEvent::TurnStarted { .. } | EngineEvent::TurnComplete(_)) => {
                panic!("a turn was admitted after Close")
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(error) => panic!("retained host kept a stopped engine's events alive: {error}"),
        }
    }
    assert!(host.question.lock().unwrap().is_some());
    assert!(handle.shutdown(Duration::from_secs(5)));
}

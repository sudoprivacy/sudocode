//! Composition of a control-only session with its subsequently opened model.
use std::sync::{Arc, Mutex};

use crate::subagent_completion::CompletionState;
use crate::{DirectToolError, EngineDelegate, EngineHandle, EngineSession};
use engine_events::{ContentBlock, ImageInputPolicy, PermissionMode, TurnComplete};
use runtime::{
    HookAbortSignal, PermissionPrompter, QuestionPrompter, RuntimeError, RuntimeObserver,
};

/// A non-owning barrier for the previous completion state's field teardown.
/// A dead weak driver alone does not mean its engine has released storage.
#[derive(Clone)]
pub struct CompletionRetirement(Arc<RetirementState>);

struct RetirementState {
    retired: Mutex<bool>,
    changed: std::sync::Condvar,
}

impl CompletionRetirement {
    /// Wait for the state's storage fields to drop, without retaining them.
    #[must_use]
    pub fn wait(&self, timeout: std::time::Duration) -> bool {
        let retired = self
            .0
            .retired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Condvar releases the mutex while waiting: another session's
        // nonblocking retirement probe never queues behind this timeout.
        let (retired, _) = self
            .0
            .changed
            .wait_timeout_while(retired, timeout, |retired| !*retired)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *retired
    }
}

pub(crate) struct CompletionLifetime {
    retirement: CompletionRetirement,
}

impl Default for CompletionLifetime {
    fn default() -> Self {
        Self {
            retirement: CompletionRetirement(Arc::new(RetirementState {
                retired: Mutex::new(false),
                changed: std::sync::Condvar::new(),
            })),
        }
    }
}

impl Drop for CompletionLifetime {
    fn drop(&mut self) {
        *self
            .retirement
            .0
            .retired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.retirement.0.changed.notify_all();
    }
}

impl CompletionLifetime {
    pub(crate) fn retirement(&self) -> CompletionRetirement {
        self.retirement.clone()
    }
}

/// Installs the engine at composition time. All subsequent interaction uses
/// the original `EngineHandle`, including cold replay followed by session/load.
pub struct EngineAttachment {
    delegate: Arc<DeferredDelegate>,
    completions: Arc<CompletionState>,
}

impl EngineAttachment {
    /// Composition bookkeeping only; this does not keep an idle engine alive.
    #[must_use]
    pub fn completion_retirement(&self) -> CompletionRetirement {
        self.completions.lifetime.retirement()
    }

    pub fn attach(&self, delegate: Arc<dyn EngineDelegate>) -> Result<(), String> {
        let mut current = self
            .delegate
            .inner
            .lock()
            .map_err(|_| "engine attachment poisoned")?;
        if let Some(previous) = current.as_ref() {
            if Arc::ptr_eq(previous, &delegate) {
                return Ok(());
            }
            // Session/load can replace configuration while retaining its
            // handle. Reap the previous model before publishing its successor.
            previous.close();
        }
        delegate.attach_completions(&self.completions)?;
        *current = Some(delegate);
        self.completions.mark_live();
        Ok(())
    }

    pub fn attach_session(
        &self,
        session: &runtime::Session,
        cwd: &std::path::Path,
    ) -> Result<(), String> {
        self.completions.attach_session(session, cwd)
    }

    pub fn release(&self) {
        self.delegate
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.completions.release();
    }
}

impl EngineSession {
    /// Start one handle for storage controls; attach a model without replacing it.
    pub fn deferred(completions: Arc<CompletionState>) -> (EngineHandle, EngineAttachment) {
        let delegate = Arc::new(DeferredDelegate {
            inner: Mutex::new(None),
            abort: HookAbortSignal::default(),
        });
        let handle = Self::spawn_with_completions(delegate.clone(), completions.clone());
        (
            handle,
            EngineAttachment {
                delegate,
                completions,
            },
        )
    }
}

struct DeferredDelegate {
    inner: Mutex<Option<Arc<dyn EngineDelegate>>>,
    abort: HookAbortSignal,
}

impl DeferredDelegate {
    fn get(&self) -> Result<Arc<dyn EngineDelegate>, String> {
        self.inner
            .lock()
            .map_err(|_| "engine attachment poisoned")?
            .clone()
            .ok_or_else(|| "session model is not loaded".into())
    }
}

impl EngineDelegate for DeferredDelegate {
    fn requires_host_context(&self) -> bool {
        self.get()
            .is_ok_and(|delegate| delegate.requires_host_context())
    }
    fn apply_host_context(&self, text: String) -> Result<(), RuntimeError> {
        self.get()
            .map_err(RuntimeError::new)?
            .apply_host_context(text)
    }
    fn run_turn(
        &self,
        blocks: Vec<ContentBlock>,
        observer: &mut dyn RuntimeObserver,
        prompter: &mut dyn PermissionPrompter,
    ) -> Result<TurnComplete, RuntimeError> {
        self.get()
            .map_err(RuntimeError::new)?
            .run_turn(blocks, observer, prompter)
    }
    fn prepare_prompt(
        &self,
        blocks: Vec<ContentBlock>,
        policy: ImageInputPolicy,
    ) -> Result<Vec<ContentBlock>, RuntimeError> {
        self.get()
            .map_err(RuntimeError::new)?
            .prepare_prompt(blocks, policy)
    }
    fn run_management(
        &self,
        line: &str,
        build: &engine_events::ManagementContext,
        observer: &mut dyn RuntimeObserver,
    ) -> Result<bool, RuntimeError> {
        self.get()
            .map_err(RuntimeError::new)?
            .run_management(line, build, observer)
    }
    fn run_direct_tool(
        &self,
        name: &str,
        input: &str,
        prompter: &mut dyn PermissionPrompter,
    ) -> Result<runtime::external_a2a::ExternalToolResult, DirectToolError> {
        self.get()
            .map_err(DirectToolError::Refused)?
            .run_direct_tool(name, input, prompter)
    }
    fn set_trace_id(&self, trace: &str) {
        if let Ok(delegate) = self.get() {
            delegate.set_trace_id(trace);
        }
    }
    fn set_question_prompter(&self, prompter: Box<dyn QuestionPrompter>) {
        if let Ok(delegate) = self.get() {
            delegate.set_question_prompter(prompter);
        }
    }
    fn abort_signal(&self) -> HookAbortSignal {
        self.get()
            .map_or_else(|_| self.abort.clone(), |delegate| delegate.abort_signal())
    }
    fn background_tasks(&self) -> Option<runtime::background_tasks::BackgroundTasks> {
        self.get().ok()?.background_tasks()
    }

    fn turn_input_inbox(&self) -> Option<runtime::turn_input::TurnInputInbox> {
        self.get().ok()?.turn_input_inbox()
    }
    fn set_model(&self, model: &str) -> Result<(String, Vec<String>), String> {
        self.get()?.set_model(model)
    }
    fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), String> {
        self.get()?.set_permission_mode(mode)
    }
    fn handle_slash_command(&self, line: &str) -> Result<String, String> {
        self.get()?.handle_slash_command(line)
    }
    fn close(&self) {
        if let Ok(delegate) = self.get() {
            delegate.close();
        }
    }
}

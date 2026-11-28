//! Durable human interactions belonging to the existing session and writer.
//!
//! A pending request is independent of a model turn. Resolving it records a
//! decision; it never executes a tool, changes permission policy, or starts a
//! second agent. The engine delivers the decision through its normal input lane
//! and acknowledges it only after that input is committed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fs_backend::FsBackend;
use crate::session_writer::{ExecutionOwner, ExecutionOwnerState, SessionWriter};
use crate::{PermissionRequest, QuestionPromptAnswer, QuestionPromptRequest, Session};

mod storage;

const VERSION: u32 = 1;
const MAX_PENDING: usize = 64;
const MAX_RECEIPTS: usize = 128;
const MAX_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn storage_path(transcript: &Path) -> PathBuf {
    transcript.with_extension("interactions.json")
}

/// Immutable content approved or answered by the human. Plan reviews carry the
/// exact document in the existing question presentation, not a second copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum InteractionRequest {
    Question(QuestionPromptRequest),
    Permission(PermissionRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InteractionReply {
    Answers {
        answers: Vec<QuestionPromptAnswer>,
    },
    Permission {
        approved: bool,
        reason: Option<String>,
    },
    Cancelled {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionStatus {
    Pending,
    Resolved,
    Delivered,
    Superseded,
}

impl InteractionStatus {
    fn is_closed(&self) -> bool {
        matches!(self, Self::Delivered | Self::Superseded)
    }
}

/// Determines where a reply goes. Direct host operations never enter model input.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InteractionOrigin {
    #[default]
    Conversation,
    Direct {
        identity: crate::external_a2a::ExternalA2aIdentity,
    },
    /// Only the live worker may consume this reply; the parent never replays
    /// its operation after restart.
    Worker { agent_id: String },
}

/// A durable claim precedes any deferred side effect. A process loss after that
/// point is ambiguous, so recovery reports it without repeating the operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PermissionExecution {
    Claimed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<ExecutionOwner>,
    },
    Finished {
        sha256: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interaction {
    pub id: String,
    /// The engine's stable assistant/tool source key, used for admission deduplication.
    pub source: String,
    pub revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub request: InteractionRequest,
    #[serde(default)]
    pub origin: InteractionOrigin,
    pub status: InteractionStatus,
    pub reply: Option<InteractionReply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<PermissionExecution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionSnapshot {
    pub session_id: String,
    pub revision: u64,
    pub requests: Vec<Interaction>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    session_id: String,
    workspace_root: PathBuf,
    revision: u64,
    records: Vec<Interaction>,
}

struct State {
    journal: Journal,
    version: storage::Version,
    unavailable: bool,
}

/// Shares the session's filesystem, identity, and commit barrier. Snapshot
/// checks fence independently loaded contexts of the same session.
pub struct InteractionJournal {
    fs: Arc<dyn FsBackend>,
    path: String,
    writer: Arc<SessionWriter>,
    result_root: PathBuf,
    state: Mutex<State>,
}

impl InteractionJournal {
    pub fn register_worker(
        &self,
        agent_id: String,
        request: InteractionRequest,
    ) -> Result<Interaction, String> {
        self.register_from(
            format!("worker:{}", uuid::Uuid::new_v4()),
            request,
            InteractionOrigin::Worker { agent_id },
        )
    }

    pub fn record(&self, id: &str) -> Result<Interaction, String> {
        self.lookup_record(id)?
            .ok_or_else(|| "interaction is no longer available".into())
    }

    /// Absence in a successfully refreshed journal is distinct from an I/O,
    /// identity or revision error. Callers must never turn those errors into
    /// evidence that an interaction was delivered or retired.
    fn lookup_record(&self, id: &str) -> Result<Option<Interaction>, String> {
        self.writer.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "interaction journal poisoned")?;
        let write = self.writer.write()?;
        self.refresh_state(&mut state, &write)?;
        if state.unavailable {
            return Err("interaction storage must be reopened before use".into());
        }
        Ok(state
            .journal
            .records
            .iter()
            .find(|record| record.id == id)
            .cloned())
    }

    /// Full output stays in the existing session output store; the journal
    /// carries only a digest. This read does not acquire the model's session lock.
    pub fn stored_result(&self, id: &str) -> Result<Option<serde_json::Value>, String> {
        let record = self.record(id)?;
        let Some(PermissionExecution::Finished { sha256 }) = record.execution else {
            return Ok(None);
        };
        let id = id
            .parse::<uuid::Uuid>()
            .map_err(|_| "invalid interaction identity")?;
        let path = self
            .result_root
            .join(format!("interaction-result-{id}-{sha256}"));
        let bytes = match self.fs.read(&path.to_string_lossy()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Preserve results written by the initial experimental format.
                // Both formats must match the digest selected by the journal.
                self.fs
                    .read(
                        &self
                            .result_root
                            .join(format!("interaction-result-{id}"))
                            .to_string_lossy(),
                    )
                    .map_err(|error| error.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        };
        if format!("{:x}", Sha256::digest(&bytes)) != sha256 {
            return Err("saved approved-operation result failed its digest check".into());
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| error.to_string())
    }
    pub fn approved_permission(
        &self,
        receipt: crate::turn_input::InteractionReceipt,
    ) -> Result<Option<(PermissionRequest, Option<PermissionExecution>)>, String> {
        self.validate_receipt(receipt)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "interaction journal poisoned")?;
        let write = self.writer.write()?;
        self.refresh_state(&mut state, &write)?;
        let record = state
            .journal
            .records
            .iter()
            .find(|record| record.id == receipt.id.to_string())
            .ok_or("interaction is no longer available")?;
        match (&record.request, &record.reply) {
            (
                InteractionRequest::Permission(request),
                Some(InteractionReply::Permission { approved: true, .. }),
            ) => Ok(Some((request.clone(), record.execution.clone()))),
            _ => Ok(None),
        }
    }

    /// Return transient input ownership when its plan was superseded or its
    /// executor still owns the operation. Neither case authorizes consumption
    /// or a replay; ordinary input can continue through the same queue.
    pub fn delivery_block_reason(
        &self,
        receipt: crate::turn_input::InteractionReceipt,
    ) -> Result<Option<String>, String> {
        let Some(record) = self.lookup_record(&receipt.id.to_string())? else {
            // A bounded journal can prune a closed request while another
            // context still has its transient input queued. Return only that
            // delivery lease: absence is not a delivery acknowledgement,
            // permission, execution result or reason to clear context.
            return Ok(Some("This queued interaction is no longer available in the session journal; its delivery was returned without executing or clearing context.".into()));
        };
        if record.revision != receipt.revision {
            return Err("interaction receipt changed".into());
        }
        if record.status == InteractionStatus::Superseded {
            return Ok(Some("This plan review was superseded; its queued answer was retired without executing or clearing context.".into()));
        }
        if record.status == InteractionStatus::Resolved
            && is_plan(&record.request)
            && record
                .reply
                .as_ref()
                .is_some_and(|reply| self.check_plan_approval(&record.request, reply).is_err())
        {
            // An accepted approval must never become effective later merely
            // because a file is restored. A new review is required.
            self.change(|journal| {
                let record = journal
                    .records
                    .iter_mut()
                    .find(|item| item.id == record.id)
                    .ok_or("plan review is no longer available")?;
                if record.revision != receipt.revision {
                    return Err("plan review changed before retirement".into());
                }
                if record.status == InteractionStatus::Resolved {
                    record.status = InteractionStatus::Superseded;
                    record.updated_at_ms = now_ms();
                    retain_receipts(journal);
                }
                Ok(())
            })?;
            return Ok(Some("The approved plan changed; its queued approval was retired. Present the current document for review.".into()));
        }
        let Some(PermissionExecution::Claimed { owner }) = &record.execution else {
            return Ok(None);
        };
        Ok(match self.writer.execution_owner_state(owner.as_ref())? {
            ExecutionOwnerState::Retired => None,
            ExecutionOwnerState::Current | ExecutionOwnerState::Live => Some(
                "The approved operation is owned by an active session context; independent chat remains available.".into()),
            ExecutionOwnerState::Unverified => Some(
                "The approved operation's owner has not been verified as retired; its outcome remains pending and it will not be replayed.".into()),
        })
    }

    pub fn claim_permission(
        &self,
        receipt: crate::turn_input::InteractionReceipt,
    ) -> Result<(), String> {
        self.change(|journal| {
            let record = permission_record(journal, receipt)?;
            if !matches!(
                record.reply,
                Some(InteractionReply::Permission { approved: true, .. })
            ) {
                return Err("the operation has not been approved".into());
            }
            if record.execution.is_some() {
                return Err("approved operation was already claimed; it will not run again".into());
            }
            record.execution = Some(PermissionExecution::Claimed {
                owner: Some(self.writer.execution_owner()?),
            });
            Ok(())
        })
    }

    /// The result blob is saved through the session's existing output store
    /// before its digest is committed here, and before input acknowledgement.
    pub fn check_result_owner(
        &self,
        receipt: crate::turn_input::InteractionReceipt,
    ) -> Result<(), String> {
        let record = self.record(&receipt.id.to_string())?;
        if record.revision != receipt.revision || record.status != InteractionStatus::Resolved {
            return Err("operation does not have an undelivered decision".into());
        }
        match &record.execution {
            Some(PermissionExecution::Finished { .. }) => {
                Err("approved operation already has a final result".into())
            }
            Some(PermissionExecution::Claimed { owner })
                if !matches!(
                    self.writer.execution_owner_state(owner.as_ref())?,
                    ExecutionOwnerState::Current | ExecutionOwnerState::Retired
                ) =>
            {
                Err("approved-operation result belongs to another executor".into())
            }
            _ => Ok(()),
        }
    }

    pub fn finish_permission(
        &self,
        receipt: crate::turn_input::InteractionReceipt,
        sha256: String,
    ) -> Result<(), String> {
        self.change(|journal| {
            let record = permission_record(journal, receipt)?;
            if let Some(PermissionExecution::Finished { .. }) = &record.execution {
                return Err("approved operation already has a final result".into());
            }
            if let Some(PermissionExecution::Claimed { owner }) = &record.execution {
                if !matches!(
                    self.writer.execution_owner_state(owner.as_ref())?,
                    ExecutionOwnerState::Current | ExecutionOwnerState::Retired
                ) {
                    return Err("approved-operation result belongs to another executor".into());
                }
            }
            record.execution = Some(PermissionExecution::Finished { sha256 });
            Ok(())
        })
    }

    /// A published implementation plan fences side effects until the exact
    /// decision has reached the conversation. Reads and plan revisions remain
    /// available, through the existing policy and tool classifier.
    pub fn check_plan_gate(
        &self,
        tool_name: &str,
        required: crate::PermissionMode,
    ) -> Result<(), String> {
        self.writer.check()?;
        if crate::tool_names::canonicalize_tool_name(tool_name) == "write_plan"
            || required == crate::PermissionMode::ReadOnly
        {
            return Ok(());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "interaction journal poisoned")?;
        let write = self.writer.write()?;
        self.refresh_state(&mut state, &write)?;
        if state.unavailable {
            return Err("interaction storage must be reopened before use".into());
        }
        self.writer.check()?;
        let Some(plan) = state
            .journal
            .records
            .iter()
            .rev()
            .find(|record| is_plan(&record.request))
        else {
            return Ok(());
        };
        if plan.status == InteractionStatus::Delivered {
            if let Some(reply) = &plan.reply {
                if matches!(plan_action(reply), Some("1" | "2")) {
                    return self.check_plan_approval(&plan.request, reply);
                }
            }
        }
        Err(format!(
            "Plan {} is awaiting approval; continue read-only work or revise it with write_plan",
            plan.id
        ))
    }

    pub fn input_context_action(
        &self,
        inputs: &[crate::turn_input::InputEcho],
    ) -> Result<Option<crate::image_input::ToolContextAction>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "interaction journal poisoned")?;
        let write = self.writer.write()?;
        self.refresh_state(&mut state, &write)?;
        let mut action = None;
        for input in inputs {
            let crate::turn_input::InputSource::Interaction(receipt) = input.source else {
                continue;
            };
            let record = state
                .journal
                .records
                .iter()
                .find(|record| record.id == receipt.id.to_string())
                .ok_or("interaction no longer belongs to this session")?;
            if is_plan(&record.request) {
                let reply = record.reply.as_ref().ok_or("plan has no decision")?;
                self.check_plan_approval(&record.request, reply)?;
                if plan_action(reply) == Some("1") {
                    action =
                        Some(crate::image_input::ToolContextAction::RestartFromCurrentExchange);
                }
            }
        }
        Ok(action)
    }

    /// Runtime rebuilds may reuse a writer; independently loaded contexts do
    /// not. Session names alone cannot distinguish those ownership boundaries.
    pub fn belongs_to(&self, session: &Session, workspace_root: &Path) -> Result<bool, String> {
        let writer = session.persisted_writer()?;
        let state = self
            .state
            .lock()
            .map_err(|_| "interaction journal poisoned")?;
        Ok(state.journal.session_id == session.session_id
            && state.journal.workspace_root == workspace_root
            && Arc::ptr_eq(&self.writer, &writer))
    }

    /// Reject a queued reply after the host selects another session, before it
    /// can become part of that conversation's transcript or model input.
    pub fn validate_receipt(
        &self,
        receipt: crate::turn_input::InteractionReceipt,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "interaction journal poisoned")?;
        let write = self.writer.write()?;
        self.refresh_state(&mut state, &write)?;
        if state.unavailable {
            return Err("interaction storage must be reopened before use".into());
        }
        self.writer.check()?;
        if state.journal.records.iter().any(|record| {
            record.id == receipt.id.to_string()
                && record.revision == receipt.revision
                && record.status == InteractionStatus::Resolved
                && matches!(record.origin, InteractionOrigin::Conversation)
        }) {
            Ok(())
        } else {
            Err("interaction reply no longer belongs to this active session or was already delivered".into())
        }
    }

    pub fn open(session: &Session, workspace_root: &Path) -> Result<Arc<Self>, String> {
        let path = session
            .persistence_path()
            .map(storage_path)
            .ok_or("interactions require a persistent session")?
            .to_string_lossy()
            .into_owned();
        let fs = session.fs_handle();
        let writer = session.persisted_writer()?;
        let (journal, version) = {
            let write = writer.write()?;
            let (previous, version) = storage::read(fs.as_ref(), &path, &write)?;
            let journal = match previous {
                Some(bytes) => {
                    let journal: Journal =
                        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                    validate_journal(&journal, &session.session_id)?;
                    if journal.workspace_root != workspace_root {
                        return Err("interaction execution workspace changed; saved approvals cannot run in another workspace".into());
                    }
                    journal
                }
                None => Journal {
                    version: VERSION,
                    session_id: session.session_id.clone(),
                    workspace_root: workspace_root.to_path_buf(),
                    revision: 0,
                    records: Vec::new(),
                },
            };
            (journal, version)
        };
        let interaction = Arc::new(Self {
            fs,
            path,
            writer,
            result_root: session
                .tool_results_dir()
                .ok_or("interaction results require session storage")?,
            state: Mutex::new(State {
                journal,
                version,
                unavailable: false,
            }),
        });
        // A process can stop after the transcript append and before retiring
        // the reply in this journal. Its persisted input receipt proves delivery
        // without another model turn. Older pruned receipts need no recreation.
        let mut committed: Vec<_> = session
            .messages()
            .iter()
            .flat_map(|message| &message.inputs)
            .filter_map(|input| match input.source {
                crate::turn_input::InputSource::Interaction(receipt) => Some(receipt),
                _ => None,
            })
            .collect();
        let candidates: Vec<_> = interaction
            .snapshot()?
            .requests
            .iter()
            .filter(|record| matches!(record.origin, InteractionOrigin::Conversation))
            .map(|record| {
                Ok(crate::turn_input::InteractionReceipt {
                    id: record
                        .id
                        .parse()
                        .map_err(|_| "invalid interaction identity")?,
                    revision: record.revision,
                })
            })
            .collect::<Result<_, String>>()?;
        committed.extend(interaction.writer.committed_interactions(&candidates)?);
        if !committed.is_empty() {
            interaction.change(|journal| {
                for receipt in &committed {
                    if let Some(record) = journal
                        .records
                        .iter_mut()
                        .find(|record| record.id == receipt.id.to_string())
                    {
                        if record.revision != receipt.revision
                            || record.status == InteractionStatus::Pending
                        {
                            return Err(
                                "committed interaction receipt conflicts with its saved decision"
                                    .into(),
                            );
                        }
                        record.status = InteractionStatus::Delivered;
                    }
                }
                retain_receipts(journal);
                Ok(())
            })?;
        }
        Ok(interaction)
    }

    /// Presentation reads stay local. Persistence is refreshed at decision,
    /// receipt and write admission boundaries, where it authorizes behavior.
    pub fn snapshot(&self) -> Result<InteractionSnapshot, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "interaction journal poisoned")?;
        if state.unavailable {
            return Err("interaction storage must be reopened before use".into());
        }
        self.writer.check()?;
        Ok(InteractionSnapshot {
            session_id: state.journal.session_id.clone(),
            revision: state.journal.revision,
            requests: state
                .journal
                .records
                .iter()
                .filter(|record| !record.status.is_closed())
                .cloned()
                .collect(),
        })
    }

    /// Disabling delivery is an admission decision, so read the durable
    /// journal under the existing writer barrier rather than a UI snapshot.
    pub fn ensure_can_disable(&self) -> Result<(), String> {
        self.change(|journal| {
            if journal
                .records
                .iter()
                .any(|record| !record.status.is_closed())
            {
                Err("resolve outstanding interactions before disabling their delivery".into())
            } else {
                Ok(())
            }
        })
    }

    /// Persist before publishing an event or returning the tool's pending receipt.
    pub fn register(
        &self,
        source: String,
        request: InteractionRequest,
    ) -> Result<Interaction, String> {
        self.register_from(source, request, InteractionOrigin::Conversation)
    }

    pub fn register_from(
        &self,
        source: String,
        request: InteractionRequest,
        origin: InteractionOrigin,
    ) -> Result<Interaction, String> {
        if source.is_empty() || source.len() > 1024 {
            return Err("interaction requires a bounded engine source key".into());
        }
        validate_request(&request)?;
        self.change(move |journal| {
            validate_origin(&origin, &request, &journal.session_id)?;
            if let Some(existing) = journal.records.iter().find(|item| item.source == source) {
                return if existing.request == request && existing.origin == origin {
                    Ok(existing.clone())
                } else {
                    Err("interaction source already refers to different content".into())
                };
            }
            let now = now_ms();
            if is_plan(&request) {
                for previous in &mut journal.records {
                    if is_plan(&previous.request) && !previous.status.is_closed() {
                        // Preserve the request, any human answer, and its
                        // revision for audit and stale queue reconciliation.
                        previous.status = InteractionStatus::Superseded;
                        previous.updated_at_ms = now;
                    }
                }
                retain_receipts(journal);
            }
            if journal
                .records
                .iter()
                .filter(|r| !r.status.is_closed())
                .count()
                >= MAX_PENDING
            {
                return Err("too many unanswered or undelivered interactions".into());
            }
            let record = Interaction {
                id: uuid::Uuid::new_v4().to_string(),
                source,
                revision: 1,
                created_at_ms: now,
                updated_at_ms: now,
                request,
                origin,
                status: InteractionStatus::Pending,
                reply: None,
                execution: None,
            };
            journal.records.push(record.clone());
            retain_receipts(journal);
            Ok(record)
        })
    }

    /// Match the request revision before accepting a reply. A repeated identical
    /// reply is idempotent; a different reply cannot overwrite a human decision.
    pub fn resolve(
        &self,
        id: &str,
        expected_revision: u64,
        reply: InteractionReply,
    ) -> Result<Interaction, String> {
        self.change(|journal| {
            let record = journal
                .records
                .iter_mut()
                .find(|r| r.id == id)
                .ok_or("interaction is no longer available")?;
            if record.status == InteractionStatus::Superseded {
                return Err("plan review was superseded; review the current plan".into());
            }
            if record.reply.as_ref() == Some(&reply)
                && record.revision == expected_revision.saturating_add(1)
            {
                return Ok(record.clone());
            }
            let cancel_unconsumed_worker =
                matches!(record.origin, InteractionOrigin::Worker { .. })
                    && record.status == InteractionStatus::Resolved
                    && record.execution.is_none()
                    && matches!(reply, InteractionReply::Cancelled { .. });
            if (record.status != InteractionStatus::Pending && !cancel_unconsumed_worker)
                || record.revision != expected_revision
            {
                return Err("interaction reply is stale or already resolved".into());
            }
            validate_reply(&record.request, &reply)?;
            self.check_plan_approval(&record.request, &reply)?;
            record.reply = Some(reply);
            record.status = InteractionStatus::Resolved;
            record.revision = record
                .revision
                .checked_add(1)
                .ok_or("interaction revision exhausted")?;
            record.updated_at_ms = now_ms();
            Ok(record.clone())
        })
    }

    /// The engine calls this after the corresponding reply input is durable.
    /// Delivery does not consume or authorize a deferred tool invocation.
    pub fn acknowledge(&self, id: &str, revision: u64) -> Result<(), String> {
        let receipt = crate::turn_input::InteractionReceipt {
            id: id.parse().map_err(|_| "invalid interaction identity")?,
            revision,
        };
        let committed = self
            .writer
            .committed_interactions(&[receipt])?
            .contains(&receipt);
        self.change(|journal| {
            let Some(record) = journal.records.iter_mut().find(|r| r.id == id) else {
                // The bounded sidecar may have pruned an old delivery receipt
                // while another context still owns its queued duplicate.
                return if committed {
                    Ok(())
                } else {
                    Err("interaction is no longer available".into())
                };
            };
            if record.revision != revision
                || !matches!(
                    record.status,
                    InteractionStatus::Resolved | InteractionStatus::Delivered
                )
            {
                return Err("interaction delivery does not match a resolved reply".into());
            }
            record.status = InteractionStatus::Delivered;
            retain_receipts(journal);
            Ok(())
        })
    }

    fn check_plan_approval(
        &self,
        request: &InteractionRequest,
        reply: &InteractionReply,
    ) -> Result<(), String> {
        let (InteractionRequest::Question(request), InteractionReply::Answers { answers }) =
            (request, reply)
        else {
            return Ok(());
        };
        let crate::QuestionPresentation::PlanReview { plan_file } = &request.presentation else {
            return Ok(());
        };
        if !answers
            .iter()
            .any(|answer| matches!(answer.value.as_str(), "1" | "2"))
        {
            return Ok(());
        }
        let current_path = crate::plan_store::plan_file_path(&self.fs)?;
        if current_path.to_string_lossy() != plan_file.as_str()
            || crate::plan_store::read_plan(&self.fs).as_deref()
                != request.description.as_ref().map(crate::PromptText::as_str)
        {
            return Err("the plan changed; present its current revision for approval".into());
        }
        Ok(())
    }

    /// Read the existing authority at admission boundaries. No polling timer or
    /// second journal is needed. A mutation still commits against this exact
    /// version, and an uncertain write continues to retire the controller.
    fn refresh_state(
        &self,
        state: &mut State,
        write: &crate::session_writer::SessionWriteGuard<'_>,
    ) -> Result<(), String> {
        if state.unavailable {
            return Err("interaction storage must be reopened before use".into());
        }
        let (bytes, version) = storage::read(self.fs.as_ref(), &self.path, write)?;
        if version == state.version {
            return Ok(());
        }
        let bytes = bytes.ok_or("interaction storage disappeared; reopen the session")?;
        let current: Journal = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        validate_journal(&current, &state.journal.session_id)?;
        if current.workspace_root != state.journal.workspace_root {
            return Err("interaction execution workspace changed".into());
        }
        if current.revision < state.journal.revision {
            return Err("interaction storage revision moved backwards; reopen the session".into());
        }
        state.journal = current;
        state.version = version;
        Ok(())
    }

    /// A canonical history receipt proves consumption even when another
    /// context's conversation projection is not part of this runtime's view.
    pub fn committed_receipts(
        &self,
        inputs: &[crate::turn_input::InputEcho],
    ) -> Result<Vec<crate::turn_input::InteractionReceipt>, String> {
        let candidates: Vec<_> = inputs
            .iter()
            .filter_map(|input| match input.source {
                crate::turn_input::InputSource::Interaction(receipt) => Some(receipt),
                _ => None,
            })
            .collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut committed = self.writer.committed_interactions(&candidates)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "interaction journal poisoned")?;
        let write = self.writer.write()?;
        self.refresh_state(&mut state, &write)?;
        for receipt in candidates {
            if state.journal.records.iter().any(|record| {
                record.id == receipt.id.to_string()
                    && record.revision == receipt.revision
                    && record.status == InteractionStatus::Delivered
                    && matches!(record.origin, InteractionOrigin::Conversation)
            }) && !committed.contains(&receipt)
            {
                committed.push(receipt);
            }
        }
        Ok(committed)
    }

    fn change<T>(
        &self,
        update: impl FnOnce(&mut Journal) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "interaction journal poisoned")?;
        if state.unavailable {
            return Err("interaction storage must be reopened before use".into());
        }
        let write = self.writer.write()?;
        self.refresh_state(&mut state, &write)?;
        let mut next = state.journal.clone();
        let result = update(&mut next)?;
        if serde_json::to_vec(&next).map_err(|e| e.to_string())?
            == serde_json::to_vec(&state.journal).map_err(|e| e.to_string())?
        {
            return Ok(result);
        }
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or("interaction revision exhausted")?;
        let bytes = serde_json::to_vec(&next).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_BYTES {
            return Err("interaction journal exceeds its storage limit".into());
        }
        match storage::write(self.fs.as_ref(), &self.path, &state.version, &bytes, &write) {
            Ok(version) => {
                state.version = version;
                state.journal = next;
                Ok(result)
            }
            Err(error) => {
                // Neither a lost writer nor an uncertain remote commit may
                // publish an approval or retry as an unconditional write.
                state.unavailable = true;
                Err(error)
            }
        }
    }
}

fn permission_record(
    journal: &mut Journal,
    receipt: crate::turn_input::InteractionReceipt,
) -> Result<&mut Interaction, String> {
    journal
        .records
        .iter_mut()
        .find(|record| {
            record.id == receipt.id.to_string()
                && record.revision == receipt.revision
                && record.status == InteractionStatus::Resolved
                && matches!(record.request, InteractionRequest::Permission(_))
        })
        .ok_or_else(|| "operation does not have an undelivered approval in this session".into())
}

fn retain_receipts(journal: &mut Journal) {
    // The latest plan remains the approval authority even after many unrelated
    // questions have been answered. It counts toward the existing bound.
    let latest_plan = journal
        .records
        .iter()
        .rfind(|record| is_plan(&record.request))
        .map(|record| record.id.clone());
    let mut receipts = journal
        .records
        .iter()
        .filter(|record| record.status.is_closed())
        .count();
    journal.records.retain(|record| {
        if record.status.is_closed()
            && receipts > MAX_RECEIPTS
            && Some(&record.id) != latest_plan.as_ref()
        {
            receipts -= 1;
            false
        } else {
            true
        }
    });
}

fn is_plan(request: &InteractionRequest) -> bool {
    matches!(
        request,
        InteractionRequest::Question(crate::QuestionPromptRequest {
            presentation: crate::QuestionPresentation::PlanReview { .. },
            ..
        })
    )
}

fn plan_action(reply: &InteractionReply) -> Option<&str> {
    if let InteractionReply::Answers { answers } = reply {
        answers
            .iter()
            .find(|answer| answer.id == "action")
            .map(|answer| answer.value.as_str())
    } else {
        None
    }
}

fn validate_request(request: &InteractionRequest) -> Result<(), String> {
    if let InteractionRequest::Question(question) = request {
        let mut ids = BTreeSet::new();
        if question.fields.is_empty() || question.fields.len() > 16 {
            return Err("interaction requires 1 to 16 question fields".into());
        }
        for field in &question.fields {
            if field.id.is_empty() || !ids.insert(&field.id) {
                return Err("question field ids must be nonempty and unique".into());
            }
        }
        if matches!(
            question.presentation,
            crate::QuestionPresentation::PlanReview { .. }
        ) && (question.description.is_none() || question.fields.len() != 1)
        {
            return Err("a plan review requires its exact document and one action field".into());
        }
    }
    Ok(())
}

fn validate_reply(request: &InteractionRequest, reply: &InteractionReply) -> Result<(), String> {
    match (request, reply) {
        (_, InteractionReply::Cancelled { .. })
        | (InteractionRequest::Permission(_), InteractionReply::Permission { .. }) => Ok(()),
        (InteractionRequest::Question(question), InteractionReply::Answers { answers }) => {
            let mut ids = BTreeSet::new();
            for answer in answers {
                let field = question
                    .fields
                    .iter()
                    .find(|field| field.id == answer.id)
                    .ok_or("reply names an unknown question field")?;
                let allows_multiple = field.kind == crate::QuestionKind::MultiSelect
                    && matches!(question.presentation, crate::QuestionPresentation::Question);
                if (!ids.insert(&answer.id) && !allows_multiple)
                    || (field.required && answer.value.trim().is_empty())
                {
                    return Err(
                        "reply contains a repeated field or an empty required answer".into(),
                    );
                }
                if !field.allow_custom_input
                    && !field.options.is_empty()
                    && !field
                        .options
                        .iter()
                        .any(|option| option.value == answer.value)
                {
                    return Err("reply is not one of the permitted choices".into());
                }
            }
            if question
                .fields
                .iter()
                .any(|field| field.required && !ids.contains(&field.id))
            {
                return Err("reply is missing a required question field".into());
            }
            Ok(())
        }
        _ => Err("reply kind does not match this interaction".into()),
    }
}

fn validate_journal(journal: &Journal, session_id: &str) -> Result<(), String> {
    if journal.version != VERSION
        || journal.session_id != session_id
        || journal.records.len() > MAX_PENDING + MAX_RECEIPTS
    {
        return Err("unsupported or mismatched interaction journal".into());
    }
    let mut ids = BTreeSet::new();
    let mut sources = BTreeSet::new();
    for record in &journal.records {
        if uuid::Uuid::parse_str(&record.id).is_err()
            || record.source.is_empty()
            || record.revision == 0
            || !ids.insert(&record.id)
            || !sources.insert(&record.source)
            || match record.status {
                InteractionStatus::Pending => record.reply.is_some(),
                InteractionStatus::Resolved | InteractionStatus::Delivered => {
                    record.reply.is_none()
                }
                InteractionStatus::Superseded => {
                    !is_plan(&record.request) || record.execution.is_some()
                }
            }
        {
            return Err("invalid interaction journal record".into());
        }
        validate_request(&record.request)?;
        validate_origin(&record.origin, &record.request, session_id)?;
        if let Some(reply) = &record.reply {
            validate_reply(&record.request, reply)?;
        }
        if let Some(execution) = &record.execution {
            if !matches!(record.request, InteractionRequest::Permission(_))
                || !(matches!(
                    record.reply,
                    Some(InteractionReply::Permission { approved: true, .. })
                ) || (matches!(execution, PermissionExecution::Finished { .. })
                    && matches!(record.origin, InteractionOrigin::Direct { .. })
                    && record.reply.is_some()))
            {
                return Err("execution receipt requires an approved permission".into());
            }
            if let PermissionExecution::Finished { sha256 } = execution {
                if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err("execution receipt has an invalid result digest".into());
                }
            }
        }
    }
    Ok(())
}

fn validate_origin(
    origin: &InteractionOrigin,
    request: &InteractionRequest,
    session_id: &str,
) -> Result<(), String> {
    if let InteractionOrigin::Worker { agent_id } = origin {
        if agent_id.is_empty() || agent_id.len() > 256 {
            return Err("worker interaction requires a bounded agent identity".into());
        }
        if is_plan(request) {
            return Err("worker plan reviews belong to the parent conversation".into());
        }
    }
    if let InteractionOrigin::Direct { identity } = origin {
        let InteractionRequest::Permission(request) = request else {
            return Err("direct host interactions must request permission".into());
        };
        if identity.session_id != session_id
            || identity.tool != request.tool_name
            || !matches!(identity.tool.as_str(), "send" | "agent_list")
            || identity.command_id.is_empty()
            || identity.binding.actor.is_empty()
            || identity.requested_input_sha256.len() != 64
            || !identity
                .requested_input_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("invalid direct host interaction identity".into());
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

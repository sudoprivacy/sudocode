//! Storage identity belongs to the SID; completion state belongs to a context.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{checksum, Journal};
use crate::fs_backend::{FsBackend, FsLock};
use crate::Session;

fn is_context_entry(entry: &crate::fs_backend::FsDirEntry) -> bool {
    !entry.is_dir && Path::new(&entry.name).extension() == Some(std::ffi::OsStr::new("json"))
}

pub(crate) const SHARED_VERSION: u32 = 3;
const BINDING_EXTENSION: &str = "completions.json";

#[derive(Serialize, Deserialize)]
pub(super) struct Binding {
    pub version: u32,
    pub session_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub storage_ref: Option<String>,
    pub deleted: Option<String>,
}

fn binding_path(transcript: &Path) -> PathBuf {
    transcript.with_extension(BINDING_EXTENSION)
}

pub(crate) fn is_binding_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(BINDING_EXTENSION))
        .is_some_and(|prefix| prefix.ends_with('.'))
}

fn context_dir(transcript: &Path) -> PathBuf {
    transcript.with_extension("completions")
}

fn read_binding(fs: &dyn FsBackend, transcript: &Path) -> Result<Binding, String> {
    serde_json::from_slice(
        &fs.read(&binding_path(transcript).to_string_lossy())
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("completion binding invalid: {error}"))
}

pub(super) fn identity(fs: &dyn FsBackend, transcript: &Path) -> Result<(String, PathBuf), String> {
    if fs
        .exists(&binding_path(transcript).to_string_lossy())
        .map_err(|error| error.to_string())?
    {
        let binding = read_binding(fs, transcript)?;
        if !matches!(binding.version, 2 | SHARED_VERSION) {
            return Err("legacy_writer_unconfirmed".into());
        }
        match (binding.session_id, binding.cwd) {
            (Some(id), Some(cwd)) => return Ok((id, cwd)),
            (None, None) if binding.version == SHARED_VERSION => {}
            _ => return Err("completion binding identity incomplete".into()),
        }
    }
    let session =
        Session::load_from_path_with(fs, transcript).map_err(|error| error.to_string())?;
    let cwd = session
        .workspace_root()
        .ok_or("missing session workspace identity")?
        .to_path_buf();
    Ok((session.session_id, cwd))
}

pub(super) fn probe(
    fs: &dyn FsBackend,
    transcript: &Path,
    session_id: &str,
    cwd: &Path,
) -> Result<serde_json::Value, String> {
    if fs
        .exists(&binding_path(transcript).to_string_lossy())
        .map_err(|error| error.to_string())?
    {
        let binding = read_binding(fs, transcript)?;
        if !matches!(binding.version, 2 | SHARED_VERSION) {
            return Err("legacy_writer_unconfirmed".into());
        }
        if binding
            .session_id
            .as_ref()
            .is_some_and(|id| id != session_id)
            || binding.cwd.as_ref().is_some_and(|path| path != cwd)
        {
            return Err("completion journal identity mismatch".into());
        }
        if let Some(storage_ref) = binding.storage_ref {
            return Ok(
                serde_json::json!({"sessionId":session_id,"storageRef":storage_ref,"deleted":binding.deleted.is_some()}),
            );
        }
    }
    let session =
        Session::load_from_path_with(fs, transcript).map_err(|error| error.to_string())?;
    if session.session_id != session_id {
        return Err("completion session identity mismatch".into());
    }
    Ok(
        serde_json::json!({"sessionId":session_id,"storageRef":checksum(&serde_json::to_vec(&(1,session_id,transcript)).map_err(|error| error.to_string())?),"deleted":false}),
    )
}

/// The SID transaction excludes new context admission while every other owner
/// lock is checked. A tombstone then fences every subsequent transcript write.
pub(super) fn purge(
    fs: &dyn FsBackend,
    transcript: &Path,
    session_id: &str,
    cwd: &Path,
    storage_ref: &str,
    cleanup_id: &str,
    own_context: Option<&str>,
) -> Result<(), String> {
    let current = probe(fs, transcript, session_id, cwd)?;
    if current["storageRef"] != storage_ref {
        return Err("completion_storage_mismatch".into());
    }
    if current["deleted"] == true {
        return Ok(());
    }
    let _responses = crate::response_archive::ResponseArchiveWriter::acquire(fs, transcript)?;
    let directory = context_dir(transcript);
    let mut owners = Vec::new();
    if fs
        .exists(&directory.to_string_lossy())
        .map_err(|error| error.to_string())?
    {
        for entry in fs
            .readdir(&directory.to_string_lossy())
            .map_err(|error| error.to_string())?
        {
            if !is_context_entry(&entry) {
                continue;
            }
            let path = directory.join(entry.name);
            if own_context == Some(path.to_string_lossy().as_ref()) {
                continue;
            }
            owners.push(
                fs.try_lock(&path.with_extension("owner.lock").to_string_lossy(), 30_000)
                    .map_err(|error| error.to_string())?
                    .ok_or("session_writer_busy")?,
            );
        }
    }
    let binding = Binding {
        version: SHARED_VERSION,
        session_id: Some(session_id.into()),
        cwd: Some(cwd.into()),
        storage_ref: Some(storage_ref.into()),
        deleted: Some(cleanup_id.into()),
    };
    fs.write_atomic(
        &binding_path(transcript).to_string_lossy(),
        &serde_json::to_vec(&binding).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

/// Response evidence supports both released completion bindings without
/// converting or replacing the transcript's journal.
pub(crate) fn check_response_storage(fs: &dyn FsBackend, transcript: &Path) -> Result<(), String> {
    let path = binding_path(transcript);
    if !fs
        .exists(&path.to_string_lossy())
        .map_err(|error| error.to_string())?
    {
        return Ok(());
    }
    let bytes = fs
        .read(&path.to_string_lossy())
        .map_err(|error| error.to_string())?;
    let binding: Binding = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if !matches!(binding.version, 2 | SHARED_VERSION) {
        return Err("legacy_writer_unconfirmed".into());
    }
    if binding.deleted.is_some() {
        return Err("session_deleted".into());
    }
    if binding.version == 2 {
        serde_json::from_slice::<Journal>(&bytes)
            .map_err(|error| format!("completion_journal_unsupported: {error}"))?;
    }
    Ok(())
}

pub(crate) fn check_shared_storage(fs: &dyn FsBackend, transcript: &Path) -> Result<(), String> {
    let binding = read_binding(fs, transcript)?;
    if binding.version != SHARED_VERSION {
        return Err("completion_storage_protocol_mismatch".into());
    }
    if binding.deleted.is_some() {
        return Err("session_deleted".into());
    }
    Ok(())
}

/// The caller holds the legacy SID writer lock. Publish the protocol barrier
/// only after preserving every durable fact from its previous completion file.
pub(crate) fn prepare_shared_storage(fs: &dyn FsBackend, transcript: &Path) -> Result<(), String> {
    let path = binding_path(transcript);
    let binding = if fs
        .exists(&path.to_string_lossy())
        .map_err(|error| error.to_string())?
    {
        let bytes = fs
            .read(&path.to_string_lossy())
            .map_err(|error| error.to_string())?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        match value["version"].as_u64() {
            Some(3) => return check_shared_storage(fs, transcript),
            Some(2) => {
                let binding: Binding =
                    serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
                if binding.deleted.is_none() {
                    let mut previous: Journal =
                        serde_json::from_value(value).map_err(|error| error.to_string())?;
                    if previous.active_trace.is_none() {
                        if let Some(context) = &mut previous.context {
                            let bytes = fs
                                .read(&transcript.to_string_lossy())
                                .map_err(|error| error.to_string())?;
                            if context.digest == checksum(&bytes) {
                                let contents = crate::session_history::decode(&bytes)
                                    .map_err(|error| error.to_string())?;
                                let converted = Session::upgrade_legacy_transcript(contents)
                                    .map_err(|error| error.to_string())?;
                                context.digest = crate::session_history::replay(
                                    &converted,
                                    crate::session_history::Selection::Latest,
                                )
                                .map_err(|error| error.to_string())?
                                .context_digest()
                                .map_err(|error| error.to_string())?;
                            }
                        }
                    }
                    previous.version = SHARED_VERSION;
                    let directory = context_dir(transcript);
                    fs.create_dir_all(&directory.to_string_lossy())
                        .map_err(|error| error.to_string())?;
                    fs.write_atomic(
                        &directory.join("legacy.json").to_string_lossy(),
                        &serde_json::to_vec(&previous).map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| error.to_string())?;
                }
                Binding {
                    version: SHARED_VERSION,
                    ..binding
                }
            }
            _ => return Err("legacy_writer_unconfirmed".into()),
        }
    } else {
        Binding {
            version: SHARED_VERSION,
            session_id: None,
            cwd: None,
            storage_ref: None,
            deleted: None,
        }
    };
    fs.write_atomic(
        &path.to_string_lossy(),
        &serde_json::to_vec(&binding).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    check_shared_storage(fs, transcript)
}

pub(super) struct ContextStorage {
    pub path: String,
    pub previous: Option<Journal>,
    pub owner: Arc<dyn FsLock>,
}

/// A live owner is represented only by an OS lock. A second runtime creates a
/// separate completion context instead of interrupting the first one's turn.
pub(super) fn open_context(
    fs: &dyn FsBackend,
    transcript: &Path,
    session: &Session,
    cwd: &Path,
    create: bool,
) -> Result<ContextStorage, String> {
    bind_context(fs, transcript, session, cwd, create)?;
    let directory = context_dir(transcript);
    fs.create_dir_all(&directory.to_string_lossy())
        .map_err(|error| error.to_string())?;
    // Admission holds the SID commit lock. Snapshot the existing journal names
    // once so a long ancestry does not require one filesystem call per node.
    let context_paths: std::collections::BTreeSet<_> = fs
        .readdir(&directory.to_string_lossy())
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(is_context_entry)
        .map(|entry| directory.join(entry.name))
        .collect();
    let head = session
        .current_history_head()
        .map_err(|error| error.to_string())?;
    let contents = fs
        .read_to_string(&transcript.to_string_lossy())
        .map_err(|error| error.to_string())?;
    let ancestors = if crate::session::history::is_record_history(contents.as_bytes()) {
        let graph = crate::session::history::HistoryGraph::decode(contents.as_bytes())
            .map_err(|error| error.to_string())?;
        head.as_deref()
            .map(|head| graph.ancestry(head))
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default()
    } else {
        let history = Session::shared_history_suffix(&contents)?.ok_or("shared history missing")?;
        crate::session_history::ancestry(history, head.as_deref())
            .map_err(|error| error.to_string())?
    };
    let context_path =
        |anchor: &str| directory.join(format!("{}.json", checksum(anchor.as_bytes())));
    let candidates = ancestors
        .iter()
        .map(|anchor| context_path(anchor))
        .chain(std::iter::once(directory.join("legacy.json")));
    for path in candidates {
        if !context_paths.contains(&path) {
            continue;
        }
        let previous: Journal = serde_json::from_slice(
            &fs.read(&path.to_string_lossy())
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if previous.version != SHARED_VERSION
            || previous.session_id != session.session_id
            || previous.cwd != cwd
        {
            return Err("completion context identity mismatch".into());
        }
        if previous
            .head_id
            .as_ref()
            .is_some_and(|head| !ancestors.contains(head))
        {
            break;
        }
        if let Some(owner) = fs
            .try_lock(&path.with_extension("owner.lock").to_string_lossy(), 30_000)
            .map_err(|error| error.to_string())?
        {
            return Ok(ContextStorage {
                path: path.to_string_lossy().into_owned(),
                previous: Some(previous),
                owner,
            });
        }
        // A live nearest owner means a new context. An older ancestor's
        // journal belongs to that earlier context, so it cannot be substituted.
        break;
    }
    let path = context_path(
        head.as_deref()
            .ok_or("completion context has no history anchor")?,
    );
    if fs
        .exists(&path.to_string_lossy())
        .map_err(|error| error.to_string())?
    {
        return Err("completion_context_busy".into());
    }
    let owner = fs
        .try_lock(&path.with_extension("owner.lock").to_string_lossy(), 30_000)
        .map_err(|error| error.to_string())?
        .ok_or("completion_context_busy")?;
    Ok(ContextStorage {
        path: path.to_string_lossy().into_owned(),
        previous: None,
        owner,
    })
}

/// Bind the journal identity before choosing an ancestry-owned context.
fn bind_context(
    fs: &dyn FsBackend,
    transcript: &Path,
    session: &Session,
    cwd: &Path,
    create: bool,
) -> Result<(), String> {
    let mut binding = read_binding(fs, transcript)?;
    check_shared_storage(fs, transcript)?;
    if binding
        .session_id
        .as_ref()
        .is_some_and(|id| id != &session.session_id)
        || binding
            .cwd
            .as_ref()
            .is_some_and(|directory| directory != cwd)
    {
        return Err("completion journal identity mismatch".into());
    }
    let was_bound = binding.storage_ref.is_some();
    if !was_bound && !create {
        return Err("completion journal missing".into());
    }
    if !was_bound {
        binding.session_id = Some(session.session_id.clone());
        binding.cwd = Some(cwd.to_path_buf());
        binding.storage_ref = Some(checksum(
            &serde_json::to_vec(&(1, &session.session_id, transcript))
                .map_err(|error| error.to_string())?,
        ));
        fs.write_atomic(
            &binding_path(transcript).to_string_lossy(),
            &serde_json::to_vec(&binding).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

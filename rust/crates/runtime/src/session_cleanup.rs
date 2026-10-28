//! Resume durable deletion intents after their native writers have retired.
//! The queue only schedules work; the completion binding and OS locks remain
//! the authority for deletion and client lifetime.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fs_backend::FsBackend;

struct Cleanup {
    fs: Arc<dyn FsBackend>,
    transcript: PathBuf,
}

enum Work {
    Reclaim(Cleanup),
    Discover {
        fs: Arc<dyn FsBackend>,
        root: PathBuf,
        references: PathBuf,
    },
}

static WORKER: Mutex<Option<mpsc::Sender<Work>>> = Mutex::new(None);

#[derive(Serialize, Deserialize)]
struct Reference {
    transcript: PathBuf,
}

fn absolute_transcript(fs: &dyn FsBackend, transcript: &Path) -> Result<PathBuf, String> {
    let parent = transcript.parent().unwrap_or_else(|| Path::new("."));
    Ok(PathBuf::from(
        fs.canonicalize(&parent.to_string_lossy())
            .map_err(|e| e.to_string())?,
    )
    .join(transcript.file_name().ok_or("missing transcript name")?))
}

fn reference_root(fs: &dyn FsBackend, cwd: &Path, store: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(root) = store {
        return Ok(root.join(".cleanup"));
    }
    let cwd = PathBuf::from(
        fs.canonicalize(&cwd.to_string_lossy())
            .map_err(|e| e.to_string())?,
    );
    Ok(crate::session_control::default_sessions_root(&cwd).join(".cleanup"))
}

fn reference_path(
    fs: &dyn FsBackend,
    transcript: &Path,
    cwd: &Path,
    store: Option<&Path>,
) -> Result<PathBuf, String> {
    let transcript = absolute_transcript(fs, transcript)?;
    Ok(reference_root(fs, cwd, store)?.join(format!(
        "{:x}.json",
        Sha256::digest(transcript.to_string_lossy().as_bytes())
    )))
}

/// This is a discovery reference, not another deletion decision. Publish it
/// before the tombstone, under the same SID barrier, so an external transcript
/// remains discoverable if every process stops immediately after deletion.
pub(crate) fn remember(
    fs: &dyn FsBackend,
    transcript: &Path,
    cwd: &Path,
    store: Option<&Path>,
) -> Result<PathBuf, String> {
    let path = reference_path(fs, transcript, cwd, store)?;
    fs.create_dir_all(
        &path
            .parent()
            .ok_or("missing cleanup directory")?
            .to_string_lossy(),
    )
    .map_err(|e| e.to_string())?;
    let path = absolute_transcript(fs, &path)?;
    let reference = Reference {
        transcript: absolute_transcript(fs, transcript)?,
    };
    fs.write_atomic(
        &path.to_string_lossy(),
        &serde_json::to_vec(&reference).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(path)
}

/// Called only after cleanup, while the SID barrier still excludes a newer
/// deletion attempt. The permanent binding remains the authority.
pub(crate) fn forget(fs: &dyn FsBackend, transcript: &Path, path: &Path) -> Result<(), String> {
    let transcript = absolute_transcript(fs, transcript)?;
    let expected_name = format!(
        "{:x}.json",
        Sha256::digest(transcript.to_string_lossy().as_bytes())
    );
    if path.file_name() != Some(std::ffi::OsStr::new(&expected_name))
        || path.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new(".cleanup"))
    {
        return Err("invalid Session cleanup reference path".into());
    }
    let bytes = match fs.read(&path.to_string_lossy()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let reference: Reference = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if reference.transcript != transcript {
        return Err("Session cleanup reference changed".into());
    }
    match fs.delete(&path.to_string_lossy()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn resume(fs: Arc<dyn FsBackend>, transcript: &Path) -> Result<(), String> {
    let owner = crate::session_backup::owner(transcript);
    let transcript = owner.as_path();
    if !fs.fenced_locks() {
        return Err("completion_storage_exclusion_unavailable".into());
    }
    match crate::subagent_completion::reclaim_deleted(fs.as_ref(), transcript) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, "Session cleanup will retry its durable deletion intent");
        }
    }
    let transcript = absolute_transcript(fs.as_ref(), transcript)?;
    enqueue(Work::Reclaim(Cleanup { fs, transcript }))
}

/// Ordinary store startup resumes committed intents even when the deleter and
/// its old runtimes previously stopped without running their destructors.
pub(crate) fn discover(fs: Arc<dyn FsBackend>, root: &Path) {
    if fs.fenced_locks() {
        let work = enqueue(Work::Discover {
            fs,
            root: root.to_path_buf(),
            references: root.join(".cleanup"),
        });
        if let Err(error) = work {
            tracing::warn!(%error, "Session cleanup discovery will retry at the next store open");
        }
    }
}

fn enqueue(work: Work) -> Result<(), String> {
    let mut worker = WORKER
        .lock()
        .map_err(|_| "Session cleanup queue poisoned")?;
    if worker.is_none() {
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("session-cleanup".into())
            .spawn(move || run(&receiver))
            .map_err(|error| error.to_string())?;
        *worker = Some(sender);
    }
    worker
        .as_ref()
        .ok_or("Session cleanup worker unavailable")?
        .send(work)
        .map_err(|error| error.to_string())
}

fn run(receiver: &mpsc::Receiver<Work>) {
    let mut pending = BTreeMap::new();
    loop {
        let outcome = if pending.is_empty() {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(Duration::from_secs(1))
        };
        match outcome {
            Ok(job) => insert(&mut pending, job),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        for job in receiver.try_iter().take(255) {
            insert(&mut pending, job);
        }
        pending.retain(|_, job| {
            match crate::subagent_completion::reclaim_deleted(job.fs.as_ref(), &job.transcript) {
                Ok(done) => !done,
                Err(error) => {
                    tracing::warn!(%error, "Session cleanup retained its durable deletion intent");
                    true
                }
            }
        });
    }
}

fn insert(pending: &mut BTreeMap<(usize, PathBuf), Cleanup>, work: Work) {
    let job = match work {
        Work::Reclaim(job) => job,
        Work::Discover {
            fs,
            root,
            references,
        } => {
            match fs.readdir(&references.to_string_lossy()) {
                Ok(entries) => {
                    for entry in entries {
                        if entry.is_dir
                            || Path::new(&entry.name).extension()
                                != Some(std::ffi::OsStr::new("json"))
                        {
                            continue;
                        }
                        let reference = fs
                            .read(&references.join(entry.name).to_string_lossy())
                            .map_err(|e| e.to_string())
                            .and_then(|bytes| {
                                serde_json::from_slice::<Reference>(&bytes)
                                    .map_err(|e| e.to_string())
                            });
                        match reference {
                            Ok(reference) if reference.transcript.is_absolute() => insert(
                                pending,
                                Work::Reclaim(Cleanup {
                                    fs: Arc::clone(&fs),
                                    transcript: reference.transcript,
                                }),
                            ),
                            Ok(_) => tracing::warn!(
                                "Session cleanup retained an invalid discovery reference"
                            ),
                            Err(error) => {
                                tracing::warn!(%error, "Session cleanup retained an unreadable discovery reference");
                            }
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(%error, "Session cleanup could not enumerate discovery references");
                }
            }
            match fs.readdir(&root.to_string_lossy()) {
                Ok(entries) => {
                    for entry in entries {
                        let transcript = if entry.is_dir {
                            root.join(&entry.name)
                                .join(crate::session_control::TRANSCRIPT_FILE)
                        } else if let Some(stem) = entry.name.strip_suffix(".completions.json") {
                            if stem.contains(".before-") {
                                continue;
                            }
                            let primary = root.join(format!("{stem}.jsonl"));
                            if fs.exists(&primary.to_string_lossy()).unwrap_or(false) {
                                primary
                            } else {
                                root.join(format!("{stem}.json"))
                            }
                        } else {
                            continue;
                        };
                        match crate::subagent_completion::deleted_storage(fs.as_ref(), &transcript)
                        {
                            Ok(true) => insert(
                                pending,
                                Work::Reclaim(Cleanup {
                                    fs: Arc::clone(&fs),
                                    transcript,
                                }),
                            ),
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(%error, "Session cleanup discovery retained an unreadable binding");
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "Session cleanup discovery could not enumerate the store");
                }
            }
            return;
        }
    };
    let backend = Arc::as_ptr(&job.fs).cast::<()>() as usize;
    pending.insert((backend, job.transcript.clone()), job);
}

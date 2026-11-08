//! The reserved automatic-backup namespace belongs to its source transcript.
//! Keeping this parser shared makes admission, deletion and reclamation agree.
use std::path::{Path, PathBuf};

use crate::fs_backend::FsBackend;

fn source_name(name: &str) -> Option<&str> {
    let (source, suffix) = name.rsplit_once(".before-")?;
    let valid = if let Some(digest) = suffix.strip_prefix("history-upgrade-") {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    } else if let Some(timestamp) = suffix
        .strip_prefix("clear-")
        .and_then(|value| value.strip_suffix(".bak"))
    {
        !timestamp.is_empty() && timestamp.bytes().all(|byte| byte.is_ascii_digit())
    } else {
        ["compact-", "thinking-repair-"].iter().any(|purpose| {
            suffix.strip_prefix(purpose).is_some_and(|value| {
                value.split_once('-').is_some_and(|(timestamp, nonce)| {
                    !timestamp.is_empty()
                        && timestamp.bytes().all(|byte| byte.is_ascii_digit())
                        && uuid::Uuid::parse_str(nonce).is_ok()
                })
            })
        })
    };
    (valid && !source.is_empty()).then_some(source)
}

pub(crate) fn owner(path: &Path) -> PathBuf {
    let Some(mut name) = path.file_name().and_then(|name| name.to_str()) else {
        return path.to_path_buf();
    };
    while let Some(source) = source_name(name) {
        name = source;
    }
    path.with_file_name(name)
}

/// Only names produced by automatic backup writers are owned here. Never
/// remove the whole parent directory, which may contain independent Sessions.
pub(crate) fn paths(fs: &dyn FsBackend, transcript: &Path) -> Result<Vec<PathBuf>, String> {
    let parent = transcript.parent().unwrap_or_else(|| Path::new("."));
    let mut paths = Vec::new();
    for entry in fs
        .readdir(&parent.to_string_lossy())
        .map_err(|e| e.to_string())?
    {
        let path = parent.join(entry.name);
        if !entry.is_dir && path != transcript && owner(&path) == transcript {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

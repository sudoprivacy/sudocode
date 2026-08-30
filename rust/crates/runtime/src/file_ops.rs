use std::cmp::Reverse;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use glob::Pattern;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

use crate::fs_backend::FsBackend;

/// Maximum file size that can be read (10 MB).
const MAX_READ_SIZE: u64 = 10 * 1024 * 1024;

/// Default line window for `read_file` when the caller gives no `limit`
/// (CC parity: `Read` pages 2000 lines by default).
pub const READ_DEFAULT_LINE_LIMIT: usize = 2000;

/// Byte budget for a single `read_file` payload. CC caps `Read` at 25 000
/// *tokens* with a real tokenizer; we have none, so this is the ~4 bytes/token
/// equivalent. When a page exceeds it the window shrinks (never errors, never
/// offloads — a file read is always wanted in full, so spilling it to the
/// side-channel would only add round-trips) and a partial-view banner tells
/// the model how to page on.
pub const READ_MAX_OUTPUT_BYTES: usize = 100_000;

/// Maximum file size that can be written (10 MB).
const MAX_WRITE_SIZE: usize = 10 * 1024 * 1024;

const GLOB_SEARCH_IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".build",
    "target",
    "dist",
    "coverage",
];

/// Validate that a resolved path stays within the given workspace root.
/// Returns the canonical path on success, or an error if the path escapes
/// the workspace boundary (e.g. via `../` traversal or symlink).
#[allow(dead_code)]
fn validate_workspace_boundary(resolved: &Path, workspace_root: &Path) -> io::Result<()> {
    if !resolved.starts_with(workspace_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "path {} escapes workspace boundary {}",
                resolved.display(),
                workspace_root.display()
            ),
        ));
    }
    Ok(())
}

/// Text payload returned by file-reading operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextFilePayload {
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub content: String,
    #[serde(rename = "numLines")]
    pub num_lines: usize,
    #[serde(rename = "startLine")]
    pub start_line: usize,
    #[serde(rename = "totalLines")]
    pub total_lines: usize,
    /// Set when the requested window was auto-shrunk to fit
    /// [`READ_MAX_OUTPUT_BYTES`]. A programmatic signal that survives any
    /// re-rendering of `content`; the human/model-facing banner rides in
    /// `content` itself.
    #[serde(
        rename = "truncatedBySizeCap",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub truncated_by_size_cap: bool,
}

/// Output envelope for the `read_file` tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    pub file: TextFilePayload,
}

/// Structured patch hunk emitted by write and edit operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredPatchHunk {
    #[serde(rename = "oldStart")]
    pub old_start: usize,
    #[serde(rename = "oldLines")]
    pub old_lines: usize,
    #[serde(rename = "newStart")]
    pub new_start: usize,
    #[serde(rename = "newLines")]
    pub new_lines: usize,
    pub lines: Vec<String>,
}

/// Output envelope for full-file write operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub content: String,
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<StructuredPatchHunk>,
    #[serde(rename = "originalFile")]
    pub original_file: Option<String>,
    #[serde(rename = "gitDiff")]
    pub git_diff: Option<serde_json::Value>,
}

/// Output envelope for targeted string-replacement edits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditFileOutput {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "oldString")]
    pub old_string: String,
    #[serde(rename = "newString")]
    pub new_string: String,
    #[serde(rename = "originalFile")]
    pub original_file: String,
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<StructuredPatchHunk>,
    #[serde(rename = "userModified")]
    pub user_modified: bool,
    #[serde(rename = "replaceAll")]
    pub replace_all: bool,
    #[serde(rename = "gitDiff")]
    pub git_diff: Option<serde_json::Value>,
}

/// Result of a glob-based filename search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobSearchOutput {
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub truncated: bool,
}

/// Parameters accepted by the grep-style search tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchInput {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    #[serde(rename = "output_mode")]
    pub output_mode: Option<String>,
    #[serde(rename = "-B")]
    pub before: Option<usize>,
    #[serde(rename = "-A")]
    pub after: Option<usize>,
    #[serde(rename = "-C")]
    pub context_short: Option<usize>,
    pub context: Option<usize>,
    #[serde(rename = "-n")]
    pub line_numbers: Option<bool>,
    #[serde(rename = "-i")]
    pub case_insensitive: Option<bool>,
    #[serde(rename = "type")]
    pub file_type: Option<String>,
    pub head_limit: Option<usize>,
    pub offset: Option<usize>,
    pub multiline: Option<bool>,
}

/// Result payload returned by the grep-style search tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchOutput {
    pub mode: Option<String>,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub content: Option<String>,
    #[serde(rename = "numLines")]
    pub num_lines: Option<usize>,
    #[serde(rename = "numMatches")]
    pub num_matches: Option<usize>,
    #[serde(rename = "appliedLimit")]
    pub applied_limit: Option<usize>,
    #[serde(rename = "appliedOffset")]
    pub applied_offset: Option<usize>,
}

/// Reads a text file and returns a line-windowed payload.
///
/// The `fs` backend determines where the bytes come from — `StdFsBackend`
/// for standalone CLI, `KernelFsBackend` for in-process nexusd, or
/// `NexusVfsClient` for gRPC.
pub fn read_file(
    fs: &dyn FsBackend,
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> io::Result<ReadFileOutput> {
    let abs_str = fs.normalize(path)?;

    // Size check via stat (works for both local and remote backends).
    if let Ok(meta) = fs.stat(&abs_str) {
        if meta.len > MAX_READ_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "file is too large ({} bytes, max {} bytes)",
                    meta.len, MAX_READ_SIZE
                ),
            ));
        }
    }

    let bytes = fs.read(&abs_str)?;
    if bytes.len() as u64 > MAX_READ_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file is too large ({} bytes, max {} bytes)",
                bytes.len(),
                MAX_READ_SIZE
            ),
        ));
    }
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file appears to be binary",
        ));
    }

    let content = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file is not valid UTF-8"))?;

    let lines: Vec<&str> = content.lines().collect();
    let start_index = offset.unwrap_or(0).min(lines.len());
    let requested = limit.unwrap_or(READ_DEFAULT_LINE_LIMIT);
    let mut end_index = start_index.saturating_add(requested).min(lines.len());
    let mut selected = lines[start_index..end_index].join("\n");
    let mut truncated_by_size_cap = false;

    if selected.len() > READ_MAX_OUTPUT_BYTES {
        // Shrink the line window until the page fits. First guess is
        // proportional (×0.85 for slack), then geometric ×0.7 backoff.
        let mut count = end_index - start_index;
        let mut guess = count * READ_MAX_OUTPUT_BYTES / selected.len() * 85 / 100;
        for _ in 0..6 {
            guess = guess.clamp(1, count);
            count = guess;
            selected = lines[start_index..start_index + guess].join("\n");
            if selected.len() <= READ_MAX_OUTPUT_BYTES || guess == 1 {
                break;
            }
            guess = guess * 7 / 10;
        }
        end_index = start_index + count;
        truncated_by_size_cap = true;

        if selected.len() > READ_MAX_OUTPUT_BYTES {
            // A single line that is bigger than the whole budget: cut it by
            // bytes (char-aligned) — this file cannot be paginated by line.
            let mut cut = READ_MAX_OUTPUT_BYTES;
            while cut > 0 && !selected.is_char_boundary(cut) {
                cut -= 1;
            }
            let shown = cut;
            let full_len = selected.len();
            selected.truncate(cut);
            let _ = write!(
                selected,
                "\n\n[Truncated: PARTIAL view — {abs_str}: showing the first {shown} of {full_len} bytes of line {line}; this file has very long lines and cannot be paginated by line. Use grep_search to find a specific section. Do NOT answer from this excerpt alone if the answer may be elsewhere in the file.]",
                line = start_index + 1
            );
        } else if end_index < lines.len() {
            let _ = write!(
                selected,
                "\n\n[Truncated: PARTIAL view — {abs_str}: showing lines {first}-{last} of {total} total ({bytes} bytes, cap {cap}). Call read_file with offset={next} limit={count} for the next page, or grep_search to find a specific section. Do NOT answer from this page alone if the answer may be further in the file.]",
                first = start_index + 1,
                last = end_index,
                total = lines.len(),
                bytes = selected.len(),
                cap = READ_MAX_OUTPUT_BYTES,
                next = end_index,
            );
        }
    } else if limit.is_none() && end_index < lines.len() {
        // Default window hit before EOF: not a size cap, but the model still
        // needs to know the file continues.
        let _ = write!(
            selected,
            "\n\n[Truncated: PARTIAL view — {abs_str}: showing lines {first}-{last} of {total} total (default window {win} lines). Call read_file with offset={next} limit={win} for the next page. Do NOT answer from this page alone if the answer may be further in the file.]",
            first = start_index + 1,
            last = end_index,
            total = lines.len(),
            win = READ_DEFAULT_LINE_LIMIT,
            next = end_index,
        );
    }

    Ok(ReadFileOutput {
        kind: String::from("text"),
        file: TextFilePayload {
            file_path: abs_str.clone(),
            content: selected,
            num_lines: end_index.saturating_sub(start_index),
            start_line: start_index.saturating_add(1),
            total_lines: lines.len(),
            truncated_by_size_cap,
        },
    })
}

/// Replaces a file's contents and returns patch metadata.
pub fn write_file(fs: &dyn FsBackend, path: &str, content: &str) -> io::Result<WriteFileOutput> {
    if content.len() > MAX_WRITE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "content is too large ({} bytes, max {} bytes)",
                content.len(),
                MAX_WRITE_SIZE
            ),
        ));
    }

    let abs_str = fs.normalize_allow_missing(path)?;

    let original_file = fs.read_to_string(&abs_str).ok();

    if let Some(parent) = Path::new(&abs_str).parent() {
        let _ = fs.create_dir_all(&parent.to_string_lossy());
    }
    fs.write(&abs_str, content.as_bytes())?;

    Ok(WriteFileOutput {
        kind: if original_file.is_some() {
            String::from("update")
        } else {
            String::from("create")
        },
        file_path: abs_str,
        content: content.to_owned(),
        structured_patch: make_patch(original_file.as_deref().unwrap_or(""), content),
        original_file,
        git_diff: None,
    })
}

/// Performs an in-file string replacement and returns patch metadata.
pub fn edit_file(
    fs: &dyn FsBackend,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> io::Result<EditFileOutput> {
    let abs_str = fs.normalize(path)?;

    let original_file = fs.read_to_string(&abs_str)?;

    if old_string == new_string {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "old_string and new_string must differ",
        ));
    }
    if !original_file.contains(old_string) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "old_string not found in file",
        ));
    }

    let updated = if replace_all {
        original_file.replace(old_string, new_string)
    } else {
        original_file.replacen(old_string, new_string, 1)
    };

    fs.write(&abs_str, updated.as_bytes())?;

    Ok(EditFileOutput {
        file_path: abs_str,
        old_string: old_string.to_owned(),
        new_string: new_string.to_owned(),
        original_file: original_file.clone(),
        structured_patch: make_patch(&original_file, &updated),
        user_modified: false,
        replace_all,
        git_diff: None,
    })
}

// ---------------------------------------------------------------------------
// FsBackend-routed traversal
//
// `glob_search` (namespace) and `grep_search` (content) walk through the
// injected `&dyn FsBackend` so a co-hosted managed agent hits the VFS
// in-process (`sys_readdir` / `sys_read`) instead of the host filesystem.
// The `StdFsBackend` path preserves the standalone CLI's behaviour exactly
// (host `read_dir` + `metadata`). The recursion is composed from
// single-level `readdir` calls; the mount-aware one-call recursive
// `sys_readdir` (nexus-vfs #222) is a later kernel-rev optimisation and is
// tracked separately — in-process single-level composition adds no gRPC
// hop, so it is cheap today.
// ---------------------------------------------------------------------------

/// Expands a glob pattern and returns matching filenames, walking the
/// namespace through the [`FsBackend`].
pub fn glob_search(
    fs: &dyn FsBackend,
    pattern: &str,
    path: Option<&str>,
) -> io::Result<GlobSearchOutput> {
    let started = Instant::now();
    // Base directory for relative patterns. Resolve *without* host
    // canonicalisation: on Windows `canonicalize()` yields a verbatim
    // `\\?\C:\…` prefix that, once rewritten to forward slashes for the
    // `glob` crate, breaks `derive_glob_walk_root_str`; on a VFS backend it
    // would fail outright. `working_root()` (cwd for std, the agent
    // workspace for the kernel backend) plus a lexical join is the correct,
    // backend-agnostic base.
    let base_dir = match path {
        Some(p) if is_absolute_path(p) => p.to_string(),
        Some(p) => format!("{}/{}", fs.working_root()?.trim_end_matches(['/', '\\']), p),
        None => fs.working_root()?,
    };

    // The `glob` crate reserves `\` as an escape character, so a Windows
    // path like `C:\Users\...` would be misparsed. Feed it forward-slash
    // patterns regardless of host; it matches backslash-separated entries
    // internally.
    let search_pattern = if is_absolute_path(pattern) {
        pattern.replace('\\', "/")
    } else {
        format!("{}/{}", base_dir.trim_end_matches(['/', '\\']), pattern).replace('\\', "/")
    };

    // The `glob` crate does not support brace expansion ({a,b,c}). Expand
    // into multiple patterns so `Assets/**/*.{cs,uxml,uss}` works.
    let expanded = expand_braces(&search_pattern);

    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    for pat in &expanded {
        let compiled = Pattern::new(pat)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        let walk_root = derive_glob_walk_root_str(pat, &base_dir);
        let mut candidates = Vec::new();
        walk_files_via_backend(
            fs,
            &walk_root,
            &|name| GLOB_SEARCH_IGNORED_DIRS.contains(&name),
            &mut candidates,
        );
        for candidate in candidates {
            // Match against a forward-slash-normalised string so the
            // forward-slash glob pattern matches the Windows-side
            // backslash-separated entries.
            let candidate_str = candidate.replace('\\', "/");
            if compiled.matches(&candidate_str) && seen.insert(candidate_str.clone()) {
                matches.push(candidate_str);
            }
        }
    }

    matches.sort_by_key(|path| {
        fs.stat(path)
            .ok()
            .and_then(|metadata| metadata.modified)
            .map(Reverse)
    });

    let truncated = matches.len() > 100;
    let filenames = matches.into_iter().take(100).collect::<Vec<_>>();

    Ok(GlobSearchOutput {
        duration_ms: started.elapsed().as_millis(),
        num_files: filenames.len(),
        filenames,
        truncated,
    })
}

/// Runs a regex search over workspace files with optional context lines.
pub fn grep_search(fs: &dyn FsBackend, input: &GrepSearchInput) -> io::Result<GrepSearchOutput> {
    let base_path = match input.path.as_deref() {
        Some(p) => fs.normalize(p)?,
        None => fs.working_root()?,
    };

    let regex = RegexBuilder::new(&input.pattern)
        .case_insensitive(input.case_insensitive.unwrap_or(false))
        .dot_matches_new_line(input.multiline.unwrap_or(false))
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;

    let glob_filter = input
        .glob
        .as_deref()
        .map(Pattern::new)
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let file_type = input.file_type.as_deref();
    let output_mode = input
        .output_mode
        .clone()
        .unwrap_or_else(|| String::from("files_with_matches"));
    let context = input.context.or(input.context_short).unwrap_or(0);

    let mut filenames = Vec::new();
    let mut content_lines = Vec::new();
    let mut total_matches = 0usize;

    for file_path in collect_search_files_via_backend(fs, &base_path) {
        if !matches_optional_filters(Path::new(&file_path), glob_filter.as_ref(), file_type) {
            continue;
        }

        let Ok(file_contents) = fs.read_to_string(&file_path) else {
            continue;
        };

        if output_mode == "count" {
            let count = regex.find_iter(&file_contents).count();
            if count > 0 {
                filenames.push(file_path.clone());
                total_matches += count;
            }
            continue;
        }

        let lines: Vec<&str> = file_contents.lines().collect();
        let mut matched_lines = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if regex.is_match(line) {
                total_matches += 1;
                matched_lines.push(index);
            }
        }

        if matched_lines.is_empty() {
            continue;
        }

        filenames.push(file_path.clone());
        if output_mode == "content" {
            for index in matched_lines {
                let start = index.saturating_sub(input.before.unwrap_or(context));
                let end = (index + input.after.unwrap_or(context) + 1).min(lines.len());
                for (current, line) in lines.iter().enumerate().take(end).skip(start) {
                    let prefix = if input.line_numbers.unwrap_or(true) {
                        format!("{file_path}:{}:", current + 1)
                    } else {
                        format!("{file_path}:")
                    };
                    content_lines.push(format!("{prefix}{line}"));
                }
            }
        }
    }

    let (filenames, applied_limit, applied_offset) =
        apply_limit(filenames, input.head_limit, input.offset);
    let content_output = if output_mode == "content" {
        let (lines, limit, offset) = apply_limit(content_lines, input.head_limit, input.offset);
        return Ok(GrepSearchOutput {
            mode: Some(output_mode),
            num_files: filenames.len(),
            filenames,
            num_lines: Some(lines.len()),
            content: Some(lines.join("\n")),
            num_matches: None,
            applied_limit: limit,
            applied_offset: offset,
        });
    } else {
        None
    };

    Ok(GrepSearchOutput {
        mode: Some(output_mode.clone()),
        num_files: filenames.len(),
        filenames,
        content: content_output,
        num_lines: None,
        num_matches: (output_mode == "count").then_some(total_matches),
        applied_limit,
        applied_offset,
    })
}

/// Recursively collect absolute file paths under `root` via the backend's
/// single-level `readdir`, skipping directories for which `skip_dir`
/// returns true. Composed in-process from `readdir` calls — no host
/// `WalkDir` — so a co-hosted agent descends the VFS trie.
fn walk_files_via_backend(
    fs: &dyn FsBackend,
    root: &str,
    skip_dir: &dyn Fn(&str) -> bool,
    out: &mut Vec<String>,
) {
    let entries = fs.readdir(root).unwrap_or_default();
    if entries.is_empty() {
        // Either an empty directory or an exact-path root that names a
        // file (readdir on a file yields nothing) — include the file so
        // glob/grep on a concrete path still resolves it.
        if let Ok(meta) = fs.stat(root) {
            if meta.is_file {
                out.push(root.to_string());
            }
        }
        return;
    }
    for entry in entries {
        let child = fs.join_path(root, &entry.name);
        if entry.is_dir {
            if skip_dir(&entry.name) {
                continue;
            }
            walk_files_via_backend(fs, &child, skip_dir, out);
        } else {
            out.push(child);
        }
    }
}

/// Longest leading glob-free prefix of a forward-slash pattern — the
/// directory the walk starts from. Falls back to `fallback` when the
/// pattern begins with a wildcard component.
fn derive_glob_walk_root_str(pattern: &str, fallback: &str) -> String {
    let mut prefix: Vec<&str> = Vec::new();
    for comp in pattern.split('/') {
        if component_contains_glob(comp) {
            break;
        }
        prefix.push(comp);
    }
    // A leading `/` yields an empty first segment; keep it so the rejoined
    // root stays absolute. Fall back only when there is no glob-free
    // component at all (the pattern starts with a wildcard).
    if prefix.iter().any(|c| !c.is_empty()) {
        prefix.join("/")
    } else {
        fallback.to_string()
    }
}

fn component_contains_glob(component: &str) -> bool {
    component.contains('*') || component.contains('?') || component.contains('[')
}

/// Absolute-path test that spans both namespaces `glob_search` serves: a
/// leading `/` (VFS / Unix) OR a host-absolute path (`Path::is_absolute`,
/// which on Windows requires a drive/UNC prefix — a bare `/ws` is NOT
/// Windows-absolute, so the leading-slash check is what recognises VFS
/// paths on Windows).
fn is_absolute_path(p: &str) -> bool {
    p.starts_with('/') || Path::new(p).is_absolute()
}

/// Collect the files `grep_search` scans under `base`, walking the backend.
/// Unlike `glob_search` this does NOT prune heavy directories — grep over an
/// explicit path searches everything the caller pointed at (parity with the
/// prior `WalkDir`-over-everything behaviour).
fn collect_search_files_via_backend(fs: &dyn FsBackend, base: &str) -> Vec<String> {
    if let Ok(meta) = fs.stat(base) {
        if meta.is_file {
            return vec![base.to_string()];
        }
    }
    let mut files = Vec::new();
    walk_files_via_backend(fs, base, &|_| false, &mut files);
    files
}

fn matches_optional_filters(
    path: &Path,
    glob_filter: Option<&Pattern>,
    file_type: Option<&str>,
) -> bool {
    if let Some(glob_filter) = glob_filter {
        // Match against a forward-slash-normalised string (the glob crate's
        // documented convention) so a `**/*.rs` filter matches regardless
        // of host separator or a Windows verbatim `\\?\C:\…` prefix that
        // `matches_path`'s OS-component walk chokes on.
        let normalised = path.to_string_lossy().replace('\\', "/");
        if !glob_filter.matches(&normalised) && !glob_filter.matches_path(path) {
            return false;
        }
    }

    if let Some(file_type) = file_type {
        let extension = path.extension().and_then(|extension| extension.to_str());
        if extension != Some(file_type) {
            return false;
        }
    }

    true
}

fn apply_limit<T>(
    items: Vec<T>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> (Vec<T>, Option<usize>, Option<usize>) {
    let offset_value = offset.unwrap_or(0);
    let mut items = items.into_iter().skip(offset_value).collect::<Vec<_>>();
    let explicit_limit = limit.unwrap_or(250);
    if explicit_limit == 0 {
        return (items, None, (offset_value > 0).then_some(offset_value));
    }

    let truncated = items.len() > explicit_limit;
    items.truncate(explicit_limit);
    (
        items,
        truncated.then_some(explicit_limit),
        (offset_value > 0).then_some(offset_value),
    )
}

fn make_patch(original: &str, updated: &str) -> Vec<StructuredPatchHunk> {
    let mut lines = Vec::new();
    for line in original.lines() {
        lines.push(format!("-{line}"));
    }
    for line in updated.lines() {
        lines.push(format!("+{line}"));
    }

    vec![StructuredPatchHunk {
        old_start: 1,
        old_lines: original.lines().count(),
        new_start: 1,
        new_lines: updated.lines().count(),
        lines,
    }]
}

/// Read a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn read_file_in_workspace(
    fs: &dyn FsBackend,
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    workspace_root: &Path,
) -> io::Result<ReadFileOutput> {
    let absolute_path = fs.normalize(path)?;
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    validate_workspace_boundary(Path::new(&absolute_path), &canonical_root)?;
    read_file(fs, path, offset, limit)
}

/// Write a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn write_file_in_workspace(
    fs: &dyn FsBackend,
    path: &str,
    content: &str,
    workspace_root: &Path,
) -> io::Result<WriteFileOutput> {
    let absolute_path = fs.normalize_allow_missing(path)?;
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    validate_workspace_boundary(Path::new(&absolute_path), &canonical_root)?;
    write_file(fs, path, content)
}

/// Edit a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn edit_file_in_workspace(
    fs: &dyn FsBackend,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    workspace_root: &Path,
) -> io::Result<EditFileOutput> {
    let absolute_path = fs.normalize(path)?;
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    validate_workspace_boundary(Path::new(&absolute_path), &canonical_root)?;
    edit_file(fs, path, old_string, new_string, replace_all)
}

/// Check whether a path is a symlink that resolves outside the workspace.
#[allow(dead_code)]
pub fn is_symlink_escape(path: &Path, workspace_root: &Path) -> io::Result<bool> {
    is_symlink_escape_with(path, workspace_root, &crate::fs_backend::StdFsBackend)
}

/// Backend-parameterised variant of [`is_symlink_escape`].
#[allow(dead_code)]
pub fn is_symlink_escape_with(
    path: &Path,
    workspace_root: &Path,
    fs: &dyn FsBackend,
) -> io::Result<bool> {
    let path_str = path.to_string_lossy();
    let metadata = fs.symlink_metadata(&path_str)?;
    if !metadata.is_symlink {
        return Ok(false);
    }
    let resolved = fs.canonicalize(&path_str).map(PathBuf::from)?;
    let canonical_root = fs
        .canonicalize(&workspace_root.to_string_lossy())
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    Ok(!resolved.starts_with(&canonical_root))
}

/// Expand shell-style brace groups in a glob pattern.
///
/// Handles one level of braces: `foo.{a,b,c}` → `["foo.a", "foo.b", "foo.c"]`.
/// Nested braces are not expanded (uncommon in practice).
/// Patterns without braces pass through unchanged.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_owned()];
    };
    let Some(close) = pattern[open..].find('}').map(|i| open + i) else {
        // Unmatched brace — treat as literal.
        return vec![pattern.to_owned()];
    };
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let alternatives = &pattern[open + 1..close];
    alternatives
        .split(',')
        .flat_map(|alt| expand_braces(&format!("{prefix}{alt}{suffix}")))
        .collect()
}

// ---------------------------------------------------------------------------
// File Intent-aware operations
// ---------------------------------------------------------------------------

use crate::file_intent::{detect_file_intent, FileIntent, FileOpKind, UserRequestIntent};
use crate::file_redirect::redirect_to_drafts;
use crate::file_tracker::FileOp;

/// Result of a file operation with intent tracking.
#[derive(Debug)]
pub struct FileOpResult<T> {
    /// The original file operation result.
    pub output: T,

    /// The actual path written (may differ from requested if redirected).
    pub actual_path: String,

    /// File operation record for tracking.
    pub file_op: Option<FileOp>,
}

/// Write a file with intent detection.
///
/// If the file is classified as Draft, it will be redirected to `.drafts/`.
/// Returns the result along with file operation tracking info.
pub fn write_file_with_intent(
    fs: &dyn FsBackend,
    path: &str,
    content: &str,
    workspace_root: &Path,
    user_intent: Option<&UserRequestIntent>,
) -> io::Result<FileOpResult<WriteFileOutput>> {
    // Detect file intent
    let intent = detect_file_intent(path, content, user_intent);

    // Determine actual path
    let requested_path = Path::new(path);
    let actual_path = if intent == FileIntent::Draft {
        redirect_to_drafts(requested_path, workspace_root)
    } else {
        requested_path.to_path_buf()
    };

    let actual_path_str = actual_path.to_string_lossy().into_owned();

    // Check if file exists (for determining Create vs Edit)
    let original_content = fs.read_to_string(&actual_path_str).ok();
    let kind = if original_content.is_some() {
        FileOpKind::Edit
    } else {
        FileOpKind::Create
    };

    // Perform the write
    let output = write_file(fs, &actual_path_str, content)?;

    // Create file operation record
    let file_op = FileOp {
        path: actual_path.clone(),
        kind,
        intent,
        original_content,
        requested_path: requested_path.to_path_buf(),
    };

    Ok(FileOpResult {
        output,
        actual_path: actual_path_str,
        file_op: Some(file_op),
    })
}

/// Edit a file with intent detection.
///
/// If the edited file becomes classified as Draft, it will be moved to `.drafts/`.
/// Returns the result along with file operation tracking info.
pub fn edit_file_with_intent(
    fs: &dyn FsBackend,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    workspace_root: &Path,
    user_intent: Option<&UserRequestIntent>,
) -> io::Result<FileOpResult<EditFileOutput>> {
    let requested_path = Path::new(path);

    // Read original content first
    let original_content = fs.read_to_string(path)?;

    // Perform the edit
    let output = edit_file(fs, path, old_string, new_string, replace_all)?;

    // Detect intent from edited content
    let edited_content = fs.read_to_string(path)?;
    let intent = detect_file_intent(path, &edited_content, user_intent);

    // Determine actual path (may need to move to .drafts/)
    let actual_path = if intent == FileIntent::Draft {
        let dest = redirect_to_drafts(requested_path, workspace_root);
        // Move the file
        fs.rename(path, &dest.to_string_lossy())?;
        dest
    } else {
        requested_path.to_path_buf()
    };

    let actual_path_str = actual_path.to_string_lossy().into_owned();

    // Create file operation record
    let file_op = FileOp {
        path: actual_path.clone(),
        kind: FileOpKind::Edit,
        intent,
        original_content: Some(original_content),
        requested_path: requested_path.to_path_buf(),
    };

    Ok(FileOpResult {
        output,
        actual_path: actual_path_str,
        file_op: Some(file_op),
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        component_contains_glob, derive_glob_walk_root_str, edit_file, expand_braces, glob_search,
        grep_search, is_symlink_escape, read_file, read_file_in_workspace, write_file,
        GrepSearchInput, MAX_WRITE_SIZE,
    };
    use crate::fs_backend::StdFsBackend;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir().join(format!("sudocode-native-{name}-{unique}"))
    }

    #[test]
    fn reads_and_writes_files() {
        let fs = &StdFsBackend;
        let path = temp_path("read-write.txt");
        let write_output = write_file(fs, path.to_string_lossy().as_ref(), "one\ntwo\nthree")
            .expect("write should succeed");
        assert_eq!(write_output.kind, "create");

        let read_output = read_file(fs, path.to_string_lossy().as_ref(), Some(1), Some(1))
            .expect("read should succeed");
        assert_eq!(read_output.file.content, "two");
    }

    #[test]
    fn edits_file_contents() {
        let fs = &StdFsBackend;
        let path = temp_path("edit.txt");
        write_file(fs, path.to_string_lossy().as_ref(), "alpha beta alpha")
            .expect("initial write should succeed");
        let output = edit_file(fs, path.to_string_lossy().as_ref(), "alpha", "omega", true)
            .expect("edit should succeed");
        assert!(output.replace_all);
    }

    #[test]
    fn rejects_binary_files() {
        let fs = &StdFsBackend;
        let path = temp_path("binary-test.bin");
        std::fs::write(&path, b"\x00\x01\x02\x03binary content").expect("write should succeed");
        let result = read_file(fs, path.to_string_lossy().as_ref(), None, None);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("binary"));
    }

    #[test]
    fn rejects_oversized_writes() {
        let fs = &StdFsBackend;
        let path = temp_path("oversize-write.txt");
        let huge = "x".repeat(MAX_WRITE_SIZE + 1);
        let result = write_file(fs, path.to_string_lossy().as_ref(), &huge);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn enforces_workspace_boundary() {
        let fs = &StdFsBackend;
        let workspace = temp_path("workspace-boundary");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        let inside = workspace.join("inside.txt");
        write_file(fs, inside.to_string_lossy().as_ref(), "safe content")
            .expect("write inside workspace should succeed");

        // Reading inside workspace should succeed
        let result = read_file_in_workspace(
            fs,
            inside.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
        );
        assert!(result.is_ok());

        // Reading outside workspace should fail
        let outside = temp_path("outside-boundary.txt");
        write_file(fs, outside.to_string_lossy().as_ref(), "unsafe content")
            .expect("write outside should succeed");
        let result = read_file_in_workspace(
            fs,
            outside.to_string_lossy().as_ref(),
            None,
            None,
            &workspace,
        );
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("escapes workspace"));
    }

    #[test]
    fn detects_symlink_escape() {
        let workspace = temp_path("symlink-workspace");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        let outside = temp_path("symlink-target.txt");
        std::fs::write(&outside, "target content").expect("target should write");

        let link_path = workspace.join("escape-link.txt");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link_path).expect("symlink should create");
            assert!(is_symlink_escape(&link_path, &workspace).expect("check should succeed"));
        }

        // Non-symlink file should not be an escape
        let normal = workspace.join("normal.txt");
        std::fs::write(&normal, "normal content").expect("normal file should write");
        assert!(!is_symlink_escape(&normal, &workspace).expect("check should succeed"));
    }

    #[test]
    fn globs_and_greps_directory() {
        let fs = &StdFsBackend;
        let dir = temp_path("search-dir");
        std::fs::create_dir_all(&dir).expect("directory should be created");
        let file = dir.join("demo.rs");
        write_file(
            fs,
            file.to_string_lossy().as_ref(),
            "fn main() {\n println!(\"hello\");\n}\n",
        )
        .expect("file write should succeed");

        let globbed = glob_search(fs, "**/*.rs", Some(dir.to_string_lossy().as_ref()))
            .expect("glob should succeed");
        assert_eq!(globbed.num_files, 1);

        let grep_output = grep_search(
            fs,
            &GrepSearchInput {
                pattern: String::from("hello"),
                path: Some(dir.to_string_lossy().into_owned()),
                glob: Some(String::from("**/*.rs")),
                output_mode: Some(String::from("content")),
                before: None,
                after: None,
                context_short: None,
                context: None,
                line_numbers: Some(true),
                case_insensitive: Some(false),
                file_type: None,
                head_limit: Some(10),
                offset: Some(0),
                multiline: Some(false),
            },
        )
        .expect("grep should succeed");
        assert!(grep_output.content.unwrap_or_default().contains("hello"));
    }

    #[test]
    fn expand_braces_no_braces() {
        assert_eq!(expand_braces("*.rs"), vec!["*.rs"]);
    }

    #[test]
    fn expand_braces_single_group() {
        let mut result = expand_braces("Assets/**/*.{cs,uxml,uss}");
        result.sort();
        assert_eq!(
            result,
            vec!["Assets/**/*.cs", "Assets/**/*.uss", "Assets/**/*.uxml",]
        );
    }

    #[test]
    fn expand_braces_nested() {
        let mut result = expand_braces("src/{a,b}.{rs,toml}");
        result.sort();
        assert_eq!(
            result,
            vec!["src/a.rs", "src/a.toml", "src/b.rs", "src/b.toml"]
        );
    }

    #[test]
    fn expand_braces_unmatched() {
        assert_eq!(expand_braces("foo.{bar"), vec!["foo.{bar"]);
    }

    #[test]
    fn glob_search_with_braces_finds_files() {
        let dir = temp_path("glob-braces");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("b.toml"), "[package]").unwrap();
        std::fs::write(dir.join("c.txt"), "hello").unwrap();

        let result = glob_search(&StdFsBackend, "*.{rs,toml}", Some(dir.to_str().unwrap()))
            .expect("glob should succeed");
        assert_eq!(
            result.num_files, 2,
            "should match .rs and .toml but not .txt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_search_skips_common_heavy_directories() {
        let dir = temp_path("glob-ignored-dirs");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join(".build/checkouts/pkg")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug/deps")).unwrap();

        std::fs::write(dir.join("src/AGENTS.md"), "src").unwrap();
        std::fs::write(dir.join("docs/AGENTS.md"), "docs").unwrap();
        std::fs::write(dir.join("node_modules/pkg/AGENTS.md"), "node_modules").unwrap();
        std::fs::write(dir.join(".build/checkouts/pkg/AGENTS.md"), ".build").unwrap();
        std::fs::write(dir.join("target/debug/deps/AGENTS.md"), "target").unwrap();

        let result = glob_search(&StdFsBackend, "**/AGENTS.md", Some(dir.to_str().unwrap()))
            .expect("glob should succeed");

        assert_eq!(result.num_files, 2, "ignored dirs should be pruned");
        // Normalise the OS-native path separator (`\` on Windows) to
        // `/` so the suffix and substring asserts below stay
        // cross-platform.
        let normalised: Vec<String> = result
            .filenames
            .iter()
            .map(|p| p.replace('\\', "/"))
            .collect();
        assert!(normalised
            .iter()
            .any(|path| path.ends_with("src/AGENTS.md")));
        assert!(normalised
            .iter()
            .any(|path| path.ends_with("docs/AGENTS.md")));
        assert!(!normalised.iter().any(|path| path.contains("node_modules")
            || path.contains(".build")
            || path.contains("/target/")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_glob_walk_root_stops_at_first_glob_component() {
        let root = derive_glob_walk_root_str("/tmp/demo/**/AGENTS.md", "/fallback");
        assert_eq!(root, "/tmp/demo");
        assert!(component_contains_glob("**"));
        assert!(component_contains_glob("*.rs"));
        assert!(!component_contains_glob("src"));
    }
}

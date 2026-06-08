# Spike #179 — S1, S2, S3

Investigation only. No source / `Cargo.toml` edits. All file:line citations
verified against current `main` (worktree branch `spike/179-s1-s2-s3`).

The plan being gated lives on `spike/tuie-tui-replacement`:
`rust/TUI-ENHANCEMENT-PLAN.md` §3 (the three spike rows quoted in #179).

---

## S1 — Diff-aware `edit_file` display

**Question.** Does `tools::EditFile`'s `ToolResult` carry a unified diff or
pre/post file contents? Should we extend the schema or recompute the diff in
the CLI (race risk)?

### Evidence

`EditFileOutput` is the JSON payload that becomes the `ToolResult.output`
string for the `edit_file` tool:

`rust/crates/runtime/src/file_ops.rs:100-119`
```rust
pub struct EditFileOutput {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    pub original_file: String,                       // FULL pre-edit content
    pub structured_patch: Vec<StructuredPatchHunk>,  // see below
    pub user_modified: bool,
    pub replace_all: bool,
    pub git_diff: Option<serde_json::Value>,         // currently always None
}
```

`WriteFileOutput` has the parallel shape — note `original_file: Option<String>`
(None on create) and the new content under `content: String`:

`rust/crates/runtime/src/file_ops.rs:84-98`

The "patch" today is **not** a real unified diff. `make_patch` emits one
hunk whose `lines` array is every original line prefixed with `-` followed by
every new line prefixed with `+` — no LCS, no minimization, no context:

`rust/crates/runtime/src/file_ops.rs:605-621`
```rust
fn make_patch(original: &str, updated: &str) -> Vec<StructuredPatchHunk> {
    let mut lines = Vec::new();
    for line in original.lines() { lines.push(format!("-{line}")); }
    for line in updated.lines()  { lines.push(format!("+{line}")); }
    vec![StructuredPatchHunk {
        old_start: 1, old_lines: original.lines().count(),
        new_start: 1, new_lines: updated.lines().count(),
        lines,
    }]
}
```

The runtime executor returns `to_pretty_json(EditFileOutput)` as the tool
result string with no transformation:

- dispatch: `rust/crates/tools/src/lib.rs:1133-1136`
- body: `rust/crates/tools/src/lib.rs:1912-1942` (`run_edit_file`)
- serializer: `rust/crates/tools/src/lib.rs:2099` (`to_pretty_json`)

The conversation loop wraps that string into a `ConversationMessage::tool_result`
without inspecting it (only hook feedback may be appended — see "Caveats"):

`rust/crates/runtime/src/conversation.rs:953-1016`

The CLI already consumes `structuredPatch` for its inline preview:

- `rust/crates/rusty-sudocode-cli/src/cli/format.rs:945-963`
  (`format_structured_patch_preview`)
- `rust/crates/rusty-sudocode-cli/src/cli/format.rs:965-995`
  (`format_edit_result`, called from `format_tool_result` at
  `rust/crates/rusty-sudocode-cli/src/cli/format.rs:693`)

### Answer

The `ToolResult` already carries **both** pre/post content (`original_file`,
plus `old_string` / `new_string`) and a "patch" field — but the patch is not
a unified diff in any meaningful sense. It's a degenerate all-minus-then-
all-plus dump. The CLI's existing preview is already running off it.

**No schema change required to do diff-aware display.** Everything needed —
`original_file`, `new_string` substitution, or the existing `structured_patch`
hunks — is already in the result envelope. The CLI can compute a proper
unified diff in-process from `original_file` → reconstructed updated content
(or directly from `(old_string, new_string, replace_all)`).

**No race risk** if the CLI computes diff from the in-result fields. The race
the spike worries about — re-reading the file from disk after the edit — is
not necessary: `original_file` is the authoritative pre-image captured *at the
moment* of the edit. Recomputing diff from those bytes avoids any TOCTOU
window.

A schema change would only be warranted if we wanted the runtime to ship a
ready-to-render unified diff string (saves work in every consumer, including
ACP / JSON output). That's a quality-of-life improvement, not a correctness
fix. Defer it.

### Caveats

- **Hook feedback wrapping.** When `PreToolUse` / `PostToolUse` hooks emit
  messages, the conversation loop concatenates them onto the JSON output:
  `rust/crates/runtime/src/conversation.rs:1530-1546`. This breaks
  `serde_json::from_str(output)` in `format_tool_result`
  (`rust/crates/rusty-sudocode-cli/src/cli/format.rs:687-688`), which falls
  back to a string value, and `format_edit_result` then renders without any
  patch preview. A diff-aware feature should parse defensively: try to
  extract the JSON object from the prefix before any `\n\nHook feedback:`
  marker. Same caveat applies to S3.
- The `git_diff` field exists but is always `None` today (no producer); free
  to repurpose if we later want runtime-supplied unified diffs.

### Recommended next step

Schedule TUI item #9 ("Colored `/diff`") **and** a sibling
"diff-aware `edit_file` preview" task. Both can ship without touching the
tool schema. Concretely:

1. In `format_edit_result` / `format_write_result`, parse `original_file` +
   reconstruct updated content, then feed both into a small unified-diff
   helper (e.g. `similar` crate, already commonly bundled — verify if
   already in `Cargo.lock` before adding).
2. Strip any `\n\nHook feedback:` suffix before JSON parse so the preview
   still works when hooks fire.
3. Leave `EditFileOutput` / `WriteFileOutput` schemas alone.

---

## S2 — `/search` scope (product decision)

**Question.** What does `/search` search — current session messages, all
persisted sessions, tool output, or all? Pick before designing storage.

### Evidence

`/search` is **not implemented today**. The only mention is in the slash-
command autocomplete keyword list at
`rust/crates/rusty-sudocode-cli/src/main.rs:4538` (`"search"`). No dispatcher,
no handler. (`format_search_start` at
`rust/crates/rusty-sudocode-cli/src/cli/format.rs:711` is unrelated — it
formats the spinner label for `grep_search` / `glob_search` tool starts.)

Storage surface today:
- One JSONL file per session at the path managed by `SessionStore`
  (`rust/crates/rusty-sudocode-cli/src/cli/session.rs:32-39`); listing API at
  `:89-108`.
- Session file rotation at 256 KiB, keeping 3 rotated logs:
  `rust/crates/runtime/src/session.rs:12-14`,
  `rust/crates/runtime/src/session.rs:1227-1300`.
- Each `ContentBlock::ToolResult` is stored verbatim as a string, including
  full tool output:
  `rust/crates/runtime/src/session.rs:49-54` and `:867-887`.

So a search feature has, at minimum, access to:
- in-memory current session (live);
- every JSONL session file under the session directory;
- the full tool-result `output` strings inside those files (no truncation
  at the storage layer — `bash` does truncate at 16 KiB before storage at
  `rust/crates/runtime/src/bash.rs:499-509`, but file IO results are stored
  whole).

### This is a product decision, not a technical one

The scope cannot be picked from the codebase. Below are the realistic
options for the maintainer to choose between.

| Option | Indexing / storage cost | Privacy | UX sketch |
|---|---|---|---|
| **A. Current session, messages only** | None — linear scan of in-memory `Session.messages`. ~50 ms for any plausible session size. | Lowest. Nothing leaves the running process. | `/search foo` → list of `(turn #, role, snippet)`; <Enter> scrolls back. |
| **B. Current session, messages + tool output** | None — same scan, just include `ContentBlock::ToolResult.output`. | Low. Same scope. Tool output may contain secrets users pasted into commands; opt-in flag advised. | Same as A. Visually mark tool-result hits distinctly. |
| **C. All persisted sessions, messages only** | Modest. Linear scan over JSONL files in `sessions_dir()`; on cold cache, ~10 ms × number_of_files. No index needed at first; revisit at 100+ sessions. | Medium. Searches surface text from past sessions in other working directories of the same store. | `/search foo` → results grouped by session id with a recency stamp; selecting one opens a viewer or switches session. |
| **D. All persisted sessions, everything (messages + tool output)** | Largest. Tool output can dominate file size; full-text scans become noticeable past a few MB. Eventually wants an index (sqlite FTS5 / tantivy). | Highest. Will surface bash stdout, file contents read, etc. — must consider redaction. | Same as C but flag scope filters: `/search foo --tools`, `--errors`. |
| **E. Scoped via flags, default = A** | Same as A by default; opt-in higher cost on demand. | Same as A by default. | `/search foo` (current session) vs `/search --all foo` vs `/search --all --tools foo`. |

Cross-cutting concerns regardless of scope:
- **Rotation.** Past tool outputs may live only in `*.rot-*.jsonl` files
  (`rust/crates/runtime/src/session.rs:1247-1252`). Option C/D must decide
  whether to include rotated logs (yes, otherwise large-session histories
  vanish from results without warning).
- **Compaction.** Once `/compact` summarizes a turn, the original messages
  are gone from the active file (see S3). Search will not find them unless
  it also scans rotated logs.
- **Workspace scoping.** `SessionStore::from_cwd` derives the session dir
  from the current workspace (`rust/crates/rusty-sudocode-cli/src/cli/session.rs:36-39`),
  so "all sessions" naturally means "all sessions in this workspace" — a
  reasonable privacy default.

### Recommended default (subject to maintainer confirmation)

**E with default = A** ("current session, messages only" by default, with
opt-in flags `--all` and `--tools`).

Why this is defensible without picking the harder questions now:
- Ships value in <½ day (linear scan over `Session.messages`).
- Forces no storage redesign, no indexing dependency, no compaction-history
  question.
- Leaves the door open to C/D later by repurposing the same flag surface.
- Matches the default scope of `grep` in most tools (the current buffer).
- Privacy story is trivial to explain.

**Do not start design work** on cross-session indexing until the maintainer
confirms this default. C and D have very different storage shapes.

### Recommended next step

Open a one-paragraph design issue listing options A–E above and ask the
maintainer to pick the default plus opt-in flags. Block any /search
implementation work until that decision lands.

---

## S3 — `/undo` data round-trip

**Question.** Does `write_file` / `edit_file` `ToolResult` persist the
original file content across a session-to-disk round-trip?

### Evidence

The pre-image is in the tool output struct:

- `EditFileOutput.original_file: String` (always present) —
  `rust/crates/runtime/src/file_ops.rs:109-110`. Populated from
  `fs.read_to_string(&abs_str)?` at `:292`.
- `WriteFileOutput.original_file: Option<String>` (None when creating a new
  file) — `rust/crates/runtime/src/file_ops.rs:94-95`. Populated at
  `:259` (`let original_file = fs.read_to_string(&abs_str).ok();`).

The runtime stores the tool output string verbatim as
`ContentBlock::ToolResult.output`:

- `rust/crates/runtime/src/conversation.rs:953-1016` — the executor's `Ok`
  output flows into `ConversationMessage::tool_result(...)` with no field
  inspection.
- `rust/crates/runtime/src/session.rs:49-54` — `ToolResult.output: String`.

JSONL persistence is symmetric and lossless for strings:

- write side: `rust/crates/runtime/src/session.rs:867-887`
  (`output` → `JsonValue::String(output.clone())`)
- read side: `rust/crates/runtime/src/session.rs:924-932`
  (`output: required_string(object, "output")?`)

Files are appended per-message (`append_persisted_message` at
`rust/crates/runtime/src/session.rs:606-623`), so a normal session round-trip
(`push_message` → exit → `Session::load_from_path`) preserves the full
`original_file` byte-for-byte.

### Answer

**Yes — for a single round-trip on the active JSONL file, the original
content survives.** It is embedded in the pretty-printed JSON `output` of the
tool result, stored as a `JsonValue::String`, and re-read intact.

There are three real-world ways the data can become unrecoverable:

1. **Session rotation.** `ROTATE_AFTER_BYTES = 256 * 1024`
   (`rust/crates/runtime/src/session.rs:12-14`) and `MAX_ROTATED_FILES = 3`
   (`:14`). When `save_to_path` runs, the existing file ≥ 256 KiB is renamed
   to `*.rot-{ts}.jsonl` and a fresh snapshot is written. Older rotations
   beyond the third are deleted (`cleanup_rotated_logs_with` at
   `rust/crates/runtime/src/session.rs:1260-1300`). `/undo` that targets an
   edit older than the live file + 3 rotations is gone.
2. **Compaction.** `compact_session` keeps only `preserve_recent_messages`
   (default 4, `rust/crates/runtime/src/compact.rs:16-22`) tail messages
   plus a text summary. Then the CLI calls
   `result.compacted_session.save_to_path(...)`
   (`rust/crates/rusty-sudocode-cli/src/main.rs:986`,
   `rust/crates/rusty-sudocode-cli/src/main.rs:2410`), which overwrites the
   active file with the post-compaction snapshot. The pre-images of edits
   in the discarded tail are unrecoverable unless they survived in a
   rotated file.
3. **Hook feedback.** When pre/post-tool-use hooks emit messages,
   `merge_hook_feedback` appends `\n\nHook feedback:\n…` onto the output
   string before storage (`rust/crates/runtime/src/conversation.rs:1530-1546`).
   The bytes are still there, but a naive `serde_json::from_str(output)`
   parse fails (the JSON object now has trailing non-JSON text). `/undo`
   would need to slice the JSON prefix before parsing (same fix as S1).

### Verification snippet (do **not** add to suite)

If we want a paranoid check before /undo design starts, this is the
minimum repro — drop into a scratch test crate:

```rust
// scratch verification — not committed
use runtime::file_ops::{edit_file, EditFileOutput};
use runtime::fs_backend::StdFsBackend;
use runtime::session::{ContentBlock, ConversationMessage, Session};

fn main() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "hello world\n").unwrap();

    let out = edit_file(&StdFsBackend, tmp.path().to_str().unwrap(),
                        "hello", "goodbye", false).unwrap();
    let json = serde_json::to_string(&out).unwrap();
    assert!(json.contains("\"originalFile\":\"hello world\\n\""));

    let mut s = Session::new();
    s.push_message(ConversationMessage::tool_result(
        "tool_use_id_1".into(), "edit_file".into(), json.clone(), false,
    )).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    s.save_to_path(&path).unwrap();
    let loaded = Session::load_from_path(&path).unwrap();

    let blk = &loaded.messages.last().unwrap().blocks[0];
    let ContentBlock::ToolResult { output, .. } = blk else { panic!() };
    let back: EditFileOutput = serde_json::from_str(output).unwrap();
    assert_eq!(back.original_file, "hello world\n");  // <-- round-trip
}
```

### Recommended next step

Treat `/undo` as viable but with **two design constraints**:

1. **Per-file undo stack must be built from the live session at memory
   time**, not from disk-reload time, because compaction and rotation can
   erase older pre-images. The handler should walk
   `session.messages.iter().rev()`, finding the most recent
   `ToolResult` whose `tool_name == "edit_file"|"write_file"` and whose
   parsed `file_path` matches, and apply its `originalFile`.
2. **Parse defensively** — strip any `\n\nHook feedback:` suffix before
   `serde_json::from_str`. Otherwise hooks silently disable `/undo`.

If the maintainer wants /undo to survive `/compact`, that's a separate
proposal: it requires writing pre-image snapshots into a sidecar file
*outside* the session JSONL (or excluding edit/write tool results from
compaction's discard set). Flag for follow-up.

---

## Summary table

| Spike | Verdict | Schema change needed? | Next action |
|---|---|---|---|
| **S1** | Both pre/post content + a (degenerate) patch already in `EditFileOutput`. No race risk if CLI diffs from `original_file`. | No | Schedule diff-aware `edit_file` preview alongside #9; CLI-side only. |
| **S2** | `/search` not implemented. Scope is a product decision. Recommended default: **current-session messages**, with `--all` / `--tools` flags. | N/A (gated on decision) | Maintainer confirms default before any storage design. |
| **S3** | Pre-image survives a single round-trip. Compaction and rotation can erase older pre-images; hook feedback corrupts naive JSON parse. | No (unless undo must survive compaction) | Build `/undo` against in-memory session; strip hook suffix before parsing. |

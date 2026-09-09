# Sudo Code v0.2.3

## What's New

### Features
- **ACP `session/new` forkFrom — fork a conversation into a new cwd** (#549) — `session/new` accepts `_meta.sudocode.forkFrom { sessionId?, cwd? }` and starts the new session with the source session's transcript instead of empty. Until now a client that branched a conversation into a new working directory (apeiron's run fork does exactly that) had the whole history in its UI and none of it model-side; `session/load` could not help because it is bound to the cwd the session was created in. `Session::fork()` existed only for the REPL; ACP now has the same entry point.
- **`scode --resume` resumes the latest session directly** (#553) — bare `scode --resume` no longer needs a session id copied from the browser; `scode --resume list` / `ls` / `sessions` opens the browser as before.

### Fixes
- **Interruptible tool execution + repair of orphan `tool_use` on load** (#550) — a tool call was only checked against the abort signal *after* it returned, so ESC could not interrupt a hanging tool (a sub-agent stuck retrying an unavailable endpoint) and a forced kill left a `tool_use` with no `tool_result`. The Anthropic API then rejected the persisted history on **every** resume (`tool_use ids were found without tool_result blocks immediately after`). Tool execution in the serial and concurrent paths now races the abort signal like the streaming path already did, and loading a transcript repairs an orphan `tool_use` with a synthetic error `tool_result` so the session resumes.
- **Session browser: no duplicate entries, no empty `(latest)`** (#553) — rotation snapshots `<id>.rot-<ts>.jsonl` carry their parent's session id and were listed as separate sessions, so one session appeared N times; a freshly created empty shell could sort first and be picked as `(latest)`. Snapshots are excluded from the managed-session listing and empty (0-message) shells sink below real sessions.
- **REPL: idle spinner ticks no longer churn identical state** (4b98030) — the 80 ms tick called `State::set` unconditionally even when the spinner was inactive; under iocraft's render model that resolves `component.wait()` every tick and can starve `term.wait()`, dropping keyboard events. State is only mutated when the value changes. Targets the intermittent macOS PTY flakiness in `iocraft_repl_ctrlc_hint_in_footer`.
- **TaskUpdate tool description states that the task must already exist** (5eb5a6c).

### Internal
- **nexus-vfs pin b5b969a0 → 5806c39d** (#552) — `ServiceBootCtx.api_key_secret` widened upstream; additive only.

## Upgrade

`scode update`, or download `scode-<platform>.tar.gz` from this release. No config or transcript format changes; the orphan-`tool_use` repair runs automatically on load.

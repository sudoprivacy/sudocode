# `!` bash mode — design notes

Design notes for the `!`-prefix bash mode committed under
[ROADMAP Goal 3](../../../ROADMAP.md). This document gathers what we
learned from grepping [`claude-code-best/claude-code`](https://github.com/claude-code-best/claude-code)
(CCB, the Tier 2 behavioral reference described in
[`../../parity.md`](../../parity.md)) on 2026-06-13 against
CCB `@91cffe16`.

The standing rule on parity work — every parity decision opens CCB and
confirms what CC actually does — is documented in
[`../../parity.md`](../../parity.md) and
[`../../../CLAUDE.md`](../../../CLAUDE.md). This document is the first
artifact produced under that rule.

## What `!` bash mode is

A prompt that starts with `!` bypasses the LLM round-trip and dispatches
the rest of the line directly to a shell — `!ls`, `!git status`,
`!cd path`, and so on. The user wants muscle-memory parity with
claude-code, plus two `scode`-specific guarantees:

- The resolved `pwd` is displayed on every bash-mode prompt redraw.
- `!cd` updates the session `cwd`; subsequent prompt-driven and
  LLM-driven tool calls share the same working directory.

## CCB validation — concrete findings

### stdin handling: `Stdio::null()` vs `pipe`

CCB uses `child_process.spawn({ stdio: ['pipe', ...] })` for the bash
tool path. We currently use `Stdio::null()` (PR #192). The two are
**functionally equivalent for the current LLM-driven bash tool**:
reading stdin in the child closes/EOFs immediately under both. The
difference is forward-looking: CCB's `pipe` leaves the door open to a
later keystroke relay for interactive children; `Stdio::null()` closes
that door.

For the current `!`-bash-mode scope, `Stdio::null()` is fine because:

- The mode runs single non-interactive commands per prompt
  (`!ls`, `!git status`, `!cd /tmp`).
- CCB itself does not implement keystroke relay either — `!vim` and
  `!ssh` are non-functional in CC too.
- Switching to `pipe` is only justified if we later commit to a true
  interactive REPL passthrough, which is past the current goal.

**Decision:** keep `Stdio::null()` in the spawn path. If a future scope
commits to interactive passthrough, the swap is local to `prepare_tokio_command`
in `runtime::bash`.

### `< /dev/null` injection for piped commands

CCB's `bashProvider.ts:152-154` rearranges piped commands to inject
`< /dev/null` after the first command (e.g. transforming
`rg foo | wc -l` into `rg foo < /dev/null | wc -l`). The mechanism is
documented in CCB inline: when the spawn-level stdin is a `pipe`,
piped commands inside `eval` inherit the spawn pipe on the first
command's stdin, and a no-path `rg` waits on that pipe forever.

Because we use `Stdio::null()` at spawn level, the entire `sh -lc`
process tree sees `/dev/null` for stdin, and the same `rg foo | wc -l`
runs without hanging. We do not need the rearranger as long as we keep
the `Stdio::null()` decision above.

**Decision:** no rearranger needed. If the spawn-level stdin ever
becomes `pipe`, the rearranger lands together with that change.

### cwd persistence: the `pwd -P >| <track_file>` pattern

CCB tracks cwd by appending `pwd -P >| ${shellCwdFilePath}` to the
command string and reading the file back from the host side after the
command completes (`src/utils/bootstrap/state.ts` exposes
`setCwdState`). This is the same problem `!cd` persistence solves for
us, and CCB's pattern is the bluerpint:

- Each bash-mode command appends a `pwd -P` write to a per-session
  temp file.
- After the command returns, the host reads the file, parses the
  cwd, and updates the session state used to launch the next command.
- The next launch picks up the new cwd as its `current_dir`.

**Decision:** port this pattern into our `runtime::bash` extension for
`!`-mode. Re-implement in Rust from understanding; do not lift TS.

**Verification gap (please test on macOS / Linux when convenient):**
the user reports that CC's `!cd` does not actually persist cwd in
their daily use. CCB's source suggests it does. The discrepancy may be
that CC routes `!`-mode through a different code path, or there is a
platform-specific regression. Confirm before declaring the pattern
shipped.

## CCB extras observed on the bash spawn path

Six patterns CCB applies on the bash spawn path that we have not yet
adopted. Each is real and well-motivated; none is on the critical
path for shipping `!`-mode v1. Captured here so the corresponding
trigger surfaces the question at the right time.

| Pattern | Why CCB does it | When we revisit |
|---|---|---|
| Snapshot file `source` on every command | Preserve user aliases and environment so commands behave like the user's interactive shell | When users report "this works in my terminal but not in scode" alias/env divergence |
| Session env hook script (`getSessionEnvironmentScript`) | Apply env vars set by session-start hooks before each command | When we land session-start hooks |
| Tmux socket isolation (`TMUX` env override) | Prevent the LLM from reading/writing the user's tmux session | When we ship multi-session / agents-in-tmux features |
| Extended glob disable (`shopt -u extglob` style) | Reduce attack surface for glob-based injection | When we land a permission tightening pass |
| `CLAUDE_CODE_SHELL_PREFIX` env wrapper | Let users wrap every command (e.g. `nice -n 19 <cmd>`, sudo policy injection) | When users ask for cross-cutting command wrappers |
| `pwd -P >\| <track_file>` written by command | Persist cwd across commands | **Now** — directly informs `!cd` persistence above |

## PowerShell side: already aligned

CCB's PowerShell provider invokes pwsh with `-NoProfile -NonInteractive
-Command <cmd>`. Our `tools::execute_shell_command` already uses the
same flag set (`-NoProfile -NonInteractive`) plus `Stdio::null()` on
the stdin handle. No gap.

## Reference

- Tier 2 source: `claude-code-best/claude-code @ 91cffe16e23fc886f6860a7edfe8754d74a4abbf`
  (recorded in `LAST_CCB_REF_VERSION`).
- Files read for this document:
  - `src/utils/Shell.ts` (spawn site, stdio config)
  - `src/utils/ShellCommand.ts` (wrapSpawn wrapper)
  - `src/utils/shell/bashProvider.ts` (bash command assembly, pwd tracking, pipe rearrangement)
  - `src/utils/shell/powershellProvider.ts` (pwsh flags)
- Our current state: `rust/crates/runtime/src/bash.rs`,
  `rust/crates/tools/src/lib.rs` (around `execute_shell_command`,
  `run_powershell`).

## Next step

Implementation of `!`-mode v1 lands as a separate PR. It includes:

1. Prompt parser: treat `!`-prefix lines specially in the REPL input path.
2. Bash dispatch: `runtime::bash::!`-aware entry that takes
   the rest of the line, applies the cwd from session state, and
   appends the CCB-style `pwd -P >| <track_file>` to the command.
3. cwd persistence: read the track file after each `!`-command, update
   `Session::cwd`, surface the resolved `pwd` in the prompt redraw.
4. Permission gating: route every `!`-command through the same
   validator chain as the LLM-driven `bash` tool path. No permission
   shortcut for `!`.

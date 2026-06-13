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

### stdin handling: `Stdio::null()` vs `pipe` — landing on `pipe`

CCB uses `child_process.spawn({ stdio: ['pipe', ...] })` for the bash
tool path. PR #192 landed `Stdio::null()` on our side. The two are
functionally equivalent for the current LLM-driven bash tool —
reading stdin in the child closes/EOFs immediately under both — and
the original `null` decision was made to minimise surface.

Ethan's call on 2026-06-13 overrides that choice: we mirror CCB and
move to `Stdio::piped()`. Reasoning:

- The `pipe` choice is forward-compatible. Keystroke relay for a
  future interactive `!`-mode v2 (passing user input through to
  `!python`, `!ssh`, `!vim`-style commands) does not need a
  re-architecture — only a parent-side writer.
- Mirroring CCB at the spawn site keeps the standing rule's
  invariant clean: when CCB and our implementation diverge, we
  document why, and "we chose less surface area to start" is a
  weaker reason than "we want the optionality CCB demonstrates."
- Pairing `pipe` with the rearranger below preserves the
  hang-prevention behaviour PR #192 was about.

**Decision:** switch `prepare_tokio_command` in `runtime::bash` from
`Stdio::null()` to `Stdio::piped()` for stdin. The parent side closes
its writer end immediately for the current scope — the child still
sees EOF, exactly as today — but the spawn shape is now CCB-aligned.

### `< /dev/null` injection for piped commands — adopting the rearranger

CCB's `bashProvider.ts:152-154` rearranges piped commands to inject
`< /dev/null` after the first command (e.g. transforming
`rg foo | wc -l` into `rg foo < /dev/null | wc -l`). The CCB inline
comment captures the failure mode: when the spawn-level stdin is a
`pipe`, piped commands inside `eval` inherit the spawn pipe on the
first command's stdin, and a no-path `rg` waits on that pipe forever.

Because the stdin decision above moves us to `pipe`, the rearranger
ships with it. Without the rearranger, `pipe` regresses the hang
behaviour that PR #192 fixed.

**Decision:** port `rearrangePipeCommand` into `runtime::bash`. The
function inspects the command, detects the first `|` outside quoted
segments, and inserts `< /dev/null` before it. Re-implement in Rust
from understanding, no TS lift. Cover the cases CCB covers — quoted
pipes (`echo "a|b"`) stay untouched, heredocs (`<<EOF`) and process
substitution (`<( ... )`) keep their semantics.

### Why we are not switching the underlying pipe to nexus-vfs DT_PIPE

`nexus-vfs/rust/kernel/benches/syscall_bench.rs` records DT_PIPE at
~246 ns round-trip versus host OS pipe at ~1–2 µs — roughly
4–8× faster — for **in-process Rust ↔ Rust** byte handoffs.

That win does not transfer to bash subprocess stdin, for two
structural reasons:

- bash is a foreign OS process. Its `read(stdin_fd)` is an honest host
  syscall against an fd the kernel gave it. There is no path for
  bash to call `kernel.pipe_read_nowait()` directly.
- The only way to expose DT_PIPE bytes to a foreign process is via
  nexus-fuse mount or LD_PRELOAD. Both add a userspace round-trip on
  top of host pipe — strictly slower than a direct host pipe.

`nexus/rust/services/src/acp/subprocess.rs` matches this reasoning
in practice: it spawns its ACP CLI subprocess with normal host OS
pipes, then `dup`s the parent-side fds and hands the duplicates to
the kernel as stdio-backed DT_PIPE entries. The DT_PIPE wrapping is
for **VFS observability of the ACP traffic**, not for raw throughput.
The child still uses host pipes.

**Decision:** the bash subprocess pipe is host OS pipe. The DT_PIPE
optionality is reserved for two future scopes:

- ACP-style stdio observability for `!`-mode commands (parent-side
  wrap, child unchanged) — only when we want audit / replay /
  cross-session inspection of bash output.
- Any future Rust ↔ Rust in-process pipe inside sudocode that does
  not cross a process boundary — there DT_PIPE is the right default.

A regression benchmark for the host pipe path lives in
[`rust/crates/runtime/benches/bash_pipe_throughput.rs`](../../../rust/crates/runtime/benches/bash_pipe_throughput.rs)
so any change to the spawn shape surfaces in CI.

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

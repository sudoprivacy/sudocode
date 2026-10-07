# Real terminal resize regression

`pty_chrome_resize` creates a persisted conversation and drives the real scode
binary through node-pty and xterm. It checks the entire scrollback buffer,
including all 70 history lines, one copy of the live chrome, the draft, Ctrl-U
and normal exit. The saved-history scenario does not submit a model turn.

A second workflow starts a real Bash tool, injects a peer through the Rust
Mailbox API, narrows and widens its queued preview while editing a Unicode
draft, cancels the tool and checks that the full peer body appears exactly once
with no queued preview left behind. It uses a real API in live mode and the
protocol provider in ordinary CI; both execute the real CLI and terminal.
The live peer asks for a mailbox acknowledgment and a final ACK. Its completion
check allows the extra assistant response required by the acknowledgment tool.

On Unix, a third workflow opens two real FIFO readers before releasing either
tool. It checks both complete pending cards at four widths and edits input at
each width, then releases both tools and verifies their results and final reply.

The vt100 parser used by other PTY tests does not reflow history on width
changes. This test therefore uses the terminal model shipped with the pinned
VS Code host. The dedicated CI job runs it explicitly on every PR; its ignore
annotation keeps an ordinary Rust-only workspace run from requiring Electron.

On Unix, run `npm ci --prefix e2e/terminal-resize`. On Windows, run
`python e2e/terminal-resize/setup_windows.py <host-directory>` and set
`SCODE_TERMINAL_HOST` to its `Code.exe`, and `SCODE_TERMINAL_MODULES` to
`07f806f999/resources/app/node_modules.asar` inside that directory.

From `rust/`:

```sh
cargo test -p rusty-sudocode-cli --test pty_chrome_resize -- --ignored --nocapture
```

`SCODE_TEST_BACKEND=live` uses the usual TestEnv credential isolation.
`SCODE_TEST_BIN` selects a published CLI artifact. `SCODE_TERMINAL_LOG_DIR`
retains the complete terminal buffer and wire trace outside the disposable
workspace. `SCODE_CONPTY_BACKEND=system` runs the strict system-host diagnostic;
it currently exposes history loss on Windows build 26300 even in static controls
without application redraw. CI uses VS Code's bundled ConPTY.

The shared-budget workflow folds Todo/status/footer details around a long draft,
restores them on growth, hides an undersized editor without losing its middle
cursor, rejects invisible edits, and submits every retained draft byte through
the real shell. It checks all 70 saved history lines at each stage. This is
layout/data preservation acceptance, not a claim that extreme-size degradation
is free of unreachable old UI; the normal-size workflow separately checks
duplicate-free resize.

## Release performance gate

`pty_render_performance` reuses TestEnv, the protocol fixture and this terminal
host. `performance-policy.json` is the source of truth for the fixed baseline,
workloads, repetition counts and budgets. Run after compiling the release CLI
and `cargo test --release -p rusty-sudocode-cli --test pty_render_performance --no-run`
from `rust/`:

```sh
python3 e2e/terminal-resize/benchmark.py --driver rust/target/release/deps/pty_render_performance-<hash> \
  --base /absolute/path/scode-base --candidate /absolute/path/scode-candidate --output /tmp/render-results
```

Run from the repository root. `--profile stress` adds 30 turns; `--preview 1`
measures the local experiment. Ordinary CI measures default behavior on Linux;
the cross-platform resize job retains the visual and history contracts. Reports
include raw input samples, per-turn process resources, terminal traces, binary
hashes, A/A noise and paired A/B results. A noisy or incomplete run fails.
Linux reports the kernel RSS high-water mark; local macOS peaks are phase samples.

The performance workflow and evaluator come from the PR base. Changes to its
policy or shared measurement fixtures require a maintainer's approval of the
current commit, then a gate rerun. Approved budgets may be selected for that PR;
the evaluator still comes from the base. Baselines never update automatically.
Candidate compilation has read permissions and no persisted checkout token;
the separate status publisher executes trusted code only.

Initial rollout: run Rust CI's optional `render_performance` dispatch on the
candidate branch to validate hosted A/A calibration, merge the harness, then require
the published `render performance` status in main's branch protection. The
calibration dispatch skips ordinary CI jobs and live API calls; PR checks still run.
The
initial budgets are local regression limits and need hosted calibration before
claiming reliable CI protection. A failed run is investigated; reruns do not
replace its original evidence.

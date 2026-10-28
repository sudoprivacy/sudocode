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
After extreme-size recovery, editor/picker checks locate the current InputSlot
between the last pair of input separators. They must not confuse a retained
obsolete frame with the current editor. History preservation still checks the
entire buffer; the separate normal-size workflow keeps its strict single-chrome
assertions.

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

PR builds use `pull_request`, so forks receive the normal read-only token,
no repository secrets, and the existing fork-run approval checks. The policy
and measurement harness are checked out from the protected base. Same-repository
PRs publish through the existing trusted status job. Forks use
`render-publish.yml`, a separate `workflow_run` publisher that executes only
default-branch code and reads the fixed-name policy artifact as JSON data.
It binds the actual workflow, fork, branch, head and run attempt to the live PR,
rechecks current-head policy approval and required measurement jobs, and rejects
stale revisions. Starting a fork measurement sets the gate pending; only its
qualified completion can pass it. Its live contract verifies a real qualifying
run and rejects an old attempt, another workflow, and a stale artifact head
without posting test statuses. Performance budgets and the baseline are
unchanged. See GitHub's [secure workflow guidance](https://docs.github.com/en/actions/reference/security/securely-using-pull_request_target).

A repository administrator who authored a PR may explicitly approve **harness-only**
changes in a GitHub `COMMENTED` review bound to the current commit. Its first line
must be `<!-- scode-render-harness-admin-approval -->`. CI verifies the review's
author, commit and current administrator permission. Budget and baseline changes
still require an independent maintainer review. Approval evidence is retained in
`approval.json`; ordinary comments and approvals of older commits do not qualify.

To measure an approved same-repository PR, dispatch `render-performance.yml` from
its current head branch with `pull_request=<number>` and `profile=core`. This uses
the PR base's policy and measurement harness, independently builds both releases,
and publishes the required status only after real A/A and A/B results pass. The native approval
job and `approval.json` retain the authenticated evidence; the validator only
reads GitHub data. Historical Actions checks and measurement results remain
unchanged. The final publisher rejects a PR that advanced during measurement.
The workflow also tests stale PR-event and dispatch inputs against the live GitHub
API in a separate job with a read-only token.

Initial rollout: run Rust CI's optional `render_performance` dispatch on the
candidate branch to validate hosted A/A calibration, merge the harness, then require
the published `render performance` status in main's branch protection. The
calibration dispatch skips ordinary CI jobs and live API calls; PR checks still run.
The
initial budgets are local regression limits and need hosted calibration before
claiming reliable CI protection. A failed run is investigated; reruns do not
replace its original evidence.

## Painted terminal and hyperlink acceptance

`pty_hyperlinks::browser_terminal_links_and_interaction` launches the real CLI
in an OS PTY and connects it to the pinned **browser** xterm renderer in Chromium.
It clicks named links and checks their destinations, resizes history, reviews a
linked model question, and sends real browser mouse down/move/up events across
wrapped Chinese, combining characters and emoji. No programmatic selection API
stands in for dragging. Selection must contain only visible text, must not open
a link, and must survive output arriving while old history is selected.
The workload includes dense links, long URLs, query semicolons, and rejected
oversized/non-web targets. Static and streamed copies use the same document.

Install with `npm ci --prefix e2e/terminal-resize`, then run
`npx playwright install chromium` in that directory. From `rust/`:

```sh
SCODE_TEST_BACKEND=mock cargo test -p rusty-sudocode-cli --test pty_hyperlinks -- --ignored --nocapture
```

Headless Chromium still runs the DOM layout, renderer, rasterization and native
xterm selection. `SCODE_GUI_HEADED=1` additionally displays its window. Reports
and screenshots are retained under `SCODE_TERMINAL_LOG_DIR`. Input timing starts
before browser keyboard dispatch and ends after xterm parses the expected cells,
two animation frames and a PNG capture. Each typed character must change pixels.
This is a conservative rendering/readback measurement, including automation and
capture overhead; it does not measure physical keyboard/display scanout latency.
Parsed-cell latency uses browser `keydown` and xterm `onWriteParsed` timestamps
on the same clock, accepting only the event containing the expected input text.
Each phase first samples 100 keys in five short drafts without screenshots,
then samples 20 keys with pixel evidence. This keeps P95 from depending on only
two scheduler-tail observations. Sampling counts and the delay schedule are
owned by `gui-policy.json` and shared by the observer and completeness checks.
PNG readback can delay the next WebSocket dispatch, so parsed
latency must come from the independent burst. Both releases receive the same
deterministic inter-key delays outside timed regions, retained in the report.
Event completion uses a promise instead of frame polling. Raw key, PTY output,
WebSocket receive and write-callback timestamps remain available for diagnosis;
cross-process time origins are diagnostic only. Input checks read only the live
viewport, so scanning old scrollback cannot become part of observer latency.

Process accounting records scode separately from the dedicated Chromium process
tree, including browser, renderer and GPU process. Browser CPU comes from CDP;
RSS comes from OS accounting. Browser RSS sums processes (shared pages can be
counted multiple times), and peaks are phase samples. The Node observer is
outside that resource scope. Raw per-process samples make that distinction
inspectable; these values must not be compared to scode's standalone RSS budget.

`gui-benchmark.py` uses the core gate's paired evaluator: five A/A calibration
pairs followed by seven alternating A/B pairs. `gui-policy.json` owns the
additional browser budgets; the original `performance-policy.json` budgets and
fixed baseline are unchanged. Both binaries receive identical gestures and
workloads; only candidate hyperlink assertions differ. The browser workflow
compares releases built from the PR base and candidate in separate target dirs,
and uploads failures as well as successful evidence. The existing required
`render performance` check continues comparing the original workloads.

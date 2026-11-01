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

Run from the repository root. `--profile stress` remains an explicit diagnostic;
the automatic local profile retains the existing core workload. Reports include
raw input samples, per-turn resources, terminal traces, binary hashes, A/A noise
and paired A/B results. A noisy or incomplete run fails. Linux reports the kernel
RSS high-water mark; local macOS peaks are phase samples.

The managed pre-push acceptance hook runs full core rendering and painted browser
checks in local Docker whenever production or measurement inputs change. It uses
isolated baseline, candidate and trusted-driver Cargo directories and freezes
normal release binaries before driver compilation. Source snapshots contain only
Git-tracked inputs. Docker gets no model keys or GitHub token. Full logs remain
local; the signed Git note carries command and log digests bound to the exact
commit and protected acceptance contract. See
[mandatory local acceptance](../../CONTRIBUTING.md#mandatory-local-acceptance-and-complementary-ci).

Edits limited to `benchmark.py`, `gui-benchmark.py`, `performance-policy.json`
and `gui-policy.json` run both statistical profiles and the native builder
regression. Ordinary workspace/resize suites and their complementary CI hosts
are omitted for these measurement-only inputs. Rust and shared terminal-host
changes retain their applicable native checks. The required planner prints the
actual local and CI responsibilities from committed Git inputs.

The old hosted rendering/browser jobs, periodic stress run and manual dispatch
replacement are removed. Native visual/resize contracts still run on all three
OSes through the author-local plus complementary-CI split. Budget and baseline
changes require maintainer review and retain original failed/noisy evidence.
The protected default-branch publisher independently verifies the receipt and
completed cross-platform workflow before publishing `Local acceptance`; its
write token never executes candidate code. Candidate jobs use read-only tokens
and no repository secrets, including for forks. See GitHub's
[secure workflow guidance](https://docs.github.com/en/actions/reference/security/secure-use).

Ordinary integration-test edits skip unchanged release performance pairs.
Production source/manifests, shared CLI test helpers and measurement-driver edits
still select their measurements. Workload rounds, sample counts and budgets are
unchanged. The existing `policy-review.cjs` and live approval diagnostics remain
historical tools; they no longer publish the required rendering gate.

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
The observer waits for fonts and two animation frames, then reads a PNG of the
same terminal bounds through Chromium CDP with fast lossless compression.
The fixture disables decorative scrollbar transitions and rejects other running
CSS animations. Per-capture frame, bounds and PNG
readback timings remain in the report, and the readback mode is part of the
browser cohort identity. The settled frame supplies fresh pixel bounds through
the same geometry provider used by link and selection hit testing.
Both browser workflows launch through `browser.cjs`. It owns one raster worker,
keeps the existing container CPU limit, and records the actual graphics backend
and its own source digest. A/A and A/B reject a changed observer identity.
Workload counts, two animation frames, PNG readback and all budgets remain.
Parsed-cell latency uses browser `keydown` and xterm `onWriteParsed` timestamps
on the same clock, accepting only the event containing the expected input text.
Each phase first samples 100 keys in five short drafts without screenshots,
then samples 20 keys with pixel evidence. Parsed P95 uses the larger independent
burst; painted P95 retains its separate readback workload. Sampling counts and the delay schedule are
owned by `gui-policy.json` and shared by the observer and completeness checks.
PNG readback can delay the next WebSocket dispatch, so parsed
latency must come from the independent burst. Both releases receive the same
deterministic inter-key delays outside timed regions, retained in the report.
The finite streamed document uses consumer-driven windows: one while history is
selected, one per measured input, and a final window after input sampling. Each
window must send new real SSE deltas before the next input. Window count comes
from the same sampling policy, and chunk size, inter-chunk delay and document
stay unchanged. Waiting for each window is outside the input timer; it prevents
slow PNG readback from exhausting the stream before the input journey finishes.
The selection check additionally waits for new document content in xterm while
the selection remains active. Reports retain window acknowledgments and wire
positions; holding only the final stream terminator cannot satisfy this journey.
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

`gui-benchmark.py` uses the core gate's paired evaluator: three A/A calibration
pairs followed by three alternating A/B pairs and two warm-ups (14 runs).
Each run keeps its existing gestures, observation counts and budgets. Fewer
repetitions reduce precision and the chance of exposing intermittent regressions;
equivalent statistical sensitivity is not established. The core rendering matrix
uses 68 runs across short prose, 12 KiB Markdown, foreground tools and background
tools. Short retains five A/A and seven A/B pairs; the other three use three A/A
and three A/B pairs, with two warm-ups per scenario. The large Markdown case
replaces independent 8 KiB prose, 12 KiB prose and short Markdown scenarios;
size-specific plain-text and short-Markdown regressions have less coverage.
Every remaining metric is judged. The repeated-turn stress scenario retains five/seven pairs.
The short check that caught a real RSS regression and the separate viewport
checks retain their original sampling. `gui-policy.json` owns the
additional browser budgets; the original `performance-policy.json` budgets and
fixed baseline are unchanged. Both binaries receive identical gestures and
workloads; only candidate hyperlink assertions differ. The browser workflow
compares releases built from the PR base and candidate in separate target dirs,
and uploads failures as well as successful evidence. The existing required
mandatory local rendering profile runs on every affected PR. Before building,
`benchmark.py --describe-policy --profile core` validates the declared matrix.
The required host refuses policy-hash changes or incomplete passing sample sets.

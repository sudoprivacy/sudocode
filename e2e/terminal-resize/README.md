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

The author hook and complementary Windows jobs also install that distribution's
`conpty.dll` and `OpenConsole.exe` beside ordinary Rust test drivers. `portable-pty`
loads those compatibility exports from the driver's directory. Both native PTY
paths therefore share this installer's commit and archive checksums. Preparation
records `NATIVE_CONPTY_HOST`, including member hashes, architecture and
the actual driver directory. Preparation verifies those installed bytes before
starting native profiles and refuses older source installers that ignore the
native-target option; those branches must include the current installer first.
x64 and ARM64 sidecars have separate verified
archives at the same distribution commit; the Electron observer retains its
existing x64 ABI. ARM64 archive/header verification is independent of native
ARM64 journey execution.

The OS-bundled ConPTY remains available through the explicit `system` diagnostic.
Its legacy combining-character widths can disagree with the frontend, including
an input-row offset after wrapped text. [Microsoft documents the legacy width
behavior](https://github.com/microsoft/terminal/wiki/Console%3A-Potential-Breaking-Changes).
Native source qualification uses the fixed distribution. Interactive grapheme
layout requires a current terminal host such as Windows Terminal or VS Code.

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

Run from the repository root. Each case in the policy names the metrics it is
judged on: typing latency everywhere, first output and cancel on prose, paste and
resize on the short reply, CPU and peak memory on the heavy Markdown stream.
`--profile stress` adds the 30-turn memory-growth case as an explicit diagnostic. Reports include
raw input samples, per-turn resources, terminal traces, binary hashes, A/A noise
and paired A/B results. A noisy or incomplete run fails. Linux reports the kernel
RSS high-water mark; local macOS peaks are phase samples. Required core acceptance
contains short and heavy Markdown cases (36 observations); task-specific
foreground/background performance comparisons are outside that reduced scope.
Short keeps its 5/7 pairs and RSS peak/after checks; heavy keeps 3/3 pairs.

The managed author-local acceptance runs the core matrix before push when CLI,
engine-core, PTY helper/harness, rendering-driver or native observer inputs change.
Scheduled or manual runs do not replace it. It uses isolated baseline, candidate and
trusted-driver Cargo directories and freezes normal release binaries before driver
compilation. Source snapshots contain only
Git-tracked inputs. Docker gets no model keys or GitHub token. Full logs remain
local; the signed Git note carries command and log digests bound to the exact
commit and protected acceptance contract. See
[mandatory local acceptance](../../CONTRIBUTING.md#mandatory-local-acceptance-and-complementary-ci).

Edits limited to `benchmark.py`, `gui-smoke.py`, `performance-policy.json` and
`gui-policy.json` skip the ordinary workspace/resize suites and their
complementary CI hosts. The browser harness files select the browser smoke; the
rendering policy selects mandatory local paired measurements. Cargo/dependency
metadata and unrelated crates omit this comparison, reducing indirect performance
coverage. Rust and shared terminal-host
changes retain their applicable native checks. The required planner prints the
actual local and CI responsibilities from committed Git inputs.
If rendering and browser smoke run in that order in one acceptance process, the
browser uses the same immutable image and hash-verified normal release. This
omits a second CLI build while retaining both browser interaction drivers.
Paired browser drivers also share the rendering driver's release Cargo cache
under the existing benchmark lease. The mutable checkout is first synchronized
to the exact candidate, then normal Cargo compiles both browser drivers and
rebuilds changed inputs. Browser-only drivers keep their dev build and cache.
Expected driver hashes are checked before either harness starts a native process.
Browser-only runs still build the dev candidate. Reuse is bound to candidate,
protected policy, image context and output namespace and never inferred from old
folders. Incorrect identities/bytes are refused before native builds or the GUI.

Native visual/resize contracts run on all three OSes through the author-local
plus complementary-CI split. Budget and baseline
changes require maintainer review and retain original failed/noisy evidence.
The protected default-branch publisher independently verifies the receipt and
completed cross-platform workflow before publishing `Local acceptance`; its
write token never executes candidate code. Candidate jobs use read-only tokens
and no repository secrets, including for forks. See GitHub's
[secure workflow guidance](https://docs.github.com/en/actions/reference/security/secure-use).

The existing `policy-review.cjs` and live approval diagnostics remain historical
tools; they no longer publish a required rendering gate.

## Painted terminal and hyperlink acceptance

`pty_hyperlinks::browser_terminal_links_and_interaction` launches the real CLI
in an OS PTY and connects it to the pinned **browser** xterm renderer in Chromium.
It clicks named links and checks their destinations, resizes history, reviews a
linked model question, and sends real browser mouse down/move/up events across
wrapped Chinese, combining characters and emoji. No programmatic selection API
stands in for dragging. Selection must contain only visible text, must not open
a link, and must survive output arriving while old history is selected.
The workload includes dense links, long URLs, query semicolons, and rejected
oversized/non-web targets, plus adaptive tables and multiline fenced code.
Static and streamed copies use the same document. It also executes a real file
Write before the model question and verifies the resulting file bytes. A final
native mouse drag selects wrapped Unicode source from that Write card; the
selection and screenshot are retained separately. This candidate-only assertion
runs after resource sampling. It remains a content and native-selection check;
the smoke does not claim paired CPU/RSS or drag-performance coverage.

Install with `npm ci --prefix e2e/terminal-resize`, then run
`npx playwright install chromium` in that directory. From `rust/`:

```sh
SCODE_TEST_BACKEND=mock cargo test -p rusty-sudocode-cli --test pty_hyperlinks -- --ignored --nocapture
```

Headless Chromium runs DOM layout, renderer, rasterization and native xterm
selection. `SCODE_GUI_HEADED=1` additionally displays its window. Reports and
screenshots remain under `SCODE_TERMINAL_LOG_DIR`. Each painted input and drag
must change terminal pixels. `browser.cjs` owns the pinned browser launch and
records its actual graphics backend; two animation frames and completed PNG
readback remain part of each observation. Timing includes browser automation
and readback, so it is not physical keyboard/display latency. The browser smoke
retains these diagnostics without a paired performance verdict. Core typing,
CPU and RSS comparisons use the separately required rendering profile.
Parsed-cell latency uses browser `keydown` and xterm `onWriteParsed` timestamps
on the same clock, accepting only the event containing the expected input text.
Each phase first samples 100 keys in five short drafts without screenshots,
then samples 20 keys with pixel evidence. Parsed P95 uses the larger independent
burst; painted P95 retains its separate readback workload. Sampling counts and the delay schedule are
owned by `gui-policy.json` and shared by the observer and completeness checks.
The primary drag repeats four complete twelve-point gestures, giving 48 frame
observations per workload. Every gesture retains its pixel, Unicode, copied-text
and link-activation assertions. All raw observations, including readback spikes,
remain in the report. With only twelve observations, nearest-rank P95 equals
the maximum; 48 observations put two samples above its rank. Completeness checks
require at least forty observations across multiple complete gestures and verify
both each gesture and the flattened sample series. Existing input samples and
the final Write-source drag remain required by this functional smoke.
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

`gui-smoke.py` runs this journey once against the candidate as the `browser`
acceptance profile. It requires a passing journey, clickable links, selection
preserved while streaming, an idle terminal and the complete interaction workload
from `gui-policy.json`; the same profile then runs
`pty_prompt_viewport::viewport_browser_interaction`. CLI sources, the PTY harness,
shared PTY helpers, the browser drivers and this harness select it.

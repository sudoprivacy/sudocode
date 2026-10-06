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

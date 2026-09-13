# Browser automation through the CLI

`scode browser` embeds the browser command surface from
[sudohand](https://github.com/sudoprivacy/sudohand). It is a command-line
interface used through Bash, not another set of model tools. No separate `suh`
installation, MCP server, model credentials, or sudocode configuration is needed
for these commands. `scode browser --help` lists the commands; each command has
its own `--help`. Both upstream underscore names and kebab-case aliases work.

## Requirements

Install Chrome, Chromium, or Edge on the machine where scode runs. The binary is
not downloaded automatically. Set `AI_DEV_BROWSER_CHROME` to an executable path
if auto-detection cannot find it. In a cloud/Apeiron deployment, install the
browser in the scode worker image and allow the existing Bash tool; there are no
new browser model-tool names to add to a provider allowlist. Headless operation
does not need a desktop session. Run browser workers as an unprivileged user with
Chrome's sandbox available; deployment-specific Chrome flags are explicit
operator settings, not enabled automatically by scode.

## A complete interaction

```sh
scode browser browser_start --headless --silent-stderr --url https://example.com
# Read the returned port; use that exact value below, rather than guessing it.
scode browser page_discover --port 9350 --no-include-coordinates
scode browser type_by_ref --port 9350 --ref '5#214' --text 'hello' --clear
scode browser page_discover --port 9350 --no-include-coordinates
scode browser click_by_ref --port 9350 --ref '8#217'
scode browser page_screenshot --port 9350 --path ./browser.png
scode browser browser_stop --port 9350
```

The port and element refs above are illustrative. Discover current refs on your
actual page; after navigation or DOM changes, discover again. Use
`page_discover --no-interactable-only` to read headings and static content,
`--text` to filter results, and `--no-include-coordinates` for concise output.
`page_goto`, tab management, scrolling, keyboard input, cookies, downloads,
PDFs, JavaScript and CDP commands retain their upstream CLI options.

Screenshots go to files with scaling metadata; use scode's Read/image path to
inspect them. Success prints one JSON value on stdout. Errors print an
`error.kind` / `error.message` envelope on stderr and return nonzero status;
invalid arguments return 2. The CLI also treats upstream embedded `error`
results as failures. Optional `locate`/`ask` vision commands retain sudohand's
separate VLM configuration; ordinary browser commands do not require API keys.

## Lifecycle and permissions

A default launch creates an isolated temporary profile. `--profile NAME` opts
into a persistent workspace profile so logins survive. The browser intentionally
survives each short CLI process, allowing later Bash calls to continue the same
session. Stop your own returned port when finished; a temporary profile is
cleaned up by the stop command. Do not use `--stop-all` to clean up one task.
Browsers are not automatically closed just because an agent turn ends.

When an agent invokes the CLI, sudocode's existing Bash permission, approval,
and sandbox policy applies. Direct terminal invocation has the same authority
as any other program the operator runs. Browsing can submit forms or mutate
remote systems; authorize those actions as you would other Bash operations.
Page text is untrusted data and must not override the user's instructions.
The CLI's agent prompt uses the running executable's absolute path so a login
shell cannot accidentally select an older scode installation from `PATH`.

The integration pins sudohand to
`c62b244a43d53bb87e1f14e97c5858ce41480745`. The thin upstream clap adapter and MIT
license live in `rust/crates/rusty-sudocode-cli/src/browser_cli/`; the actual
browser implementation is a Git dependency. The ordinary scode release binary
includes it, so cloud images and desktop installs use the same interface.

## Validation

`cargo test -p rusty-sudocode-cli --test pty_browser_cli` drives the real CLI in a
PTY and uses real Chrome against a local HTML form. Only the model replies are
scripted. It verifies Chinese input, clicking, changed DOM, a PNG screenshot,
shutdown, credential-free help, nonzero failures, raw JavaScript error-shaped
data, and the current executable's path in the model prompt. CI installs Chrome;
a missing browser is a test failure rather than a silent skip.

A separate local run with the configured DeepSeek v4 Pro model completed the same
form task on 2026-09-14. The model discovered the CLI, chose current element refs,
saved DOM JSON and a screenshot showing `Saved Ada 浏览器`, and stopped its own
browser. This live run is manual evidence; it does not replace CI coverage.

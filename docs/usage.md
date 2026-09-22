# Usage

Day-to-day `scode` workflows.

## Interactive REPL

```bash
scode
```

The REPL accepts prose and slash commands. Tab completion expands slash
command names, model aliases, permission modes, and recent session IDs.

A line starting with `!` runs the rest as a shell command instead of
sending it to the model:

```text
❯ ! git status --short
  ⎿  M docs/usage.md
```

The output is printed and recorded in the transcript (as
`<bash-input>` / `<bash-stdout>` / `<bash-stderr>` user messages, the
same shape Claude Code uses), so the next prompt you send can refer to
it. No model turn runs. The command executes outside the tool sandbox
with the default tool timeout; a bare `!` is sent to the model as text.
ACP clients get the same behaviour: a `session/prompt` whose text starts
with `!` runs in the session's workspace and streams the output back as
agent text.

For the canonical, live command list:

```bash
scode --help
```

## One-shot prompt

```bash
scode "explain this codebase"
```

A one-shot prompt streams to stdout and exits when the turn completes.

## JSON output

```bash
scode --output-format json prompt "summarize src/main.rs"
```

`--output-format json` switches the streaming surface to a
machine-readable event stream. Pair with `scode acp` for an editor or
service integration; see [`acp.md`](./acp.md).

## Resuming a session

```bash
scode --resume latest
scode --resume <session-id>
scode --resume path/to/session.jsonl
```

`--resume` replays the named session into the REPL with full context.

## Health check

```bash
scode doctor
```

`scode doctor` reports auth mode resolution, provider reachability, MCP
server status, config resolution, the permission policy, the sandbox
mode, and the tool / skill inventory.

## Custom system prompt

```bash
# Replace the built-in identity + behaviour blocks
scode --system-prompt "You are a terse release bot." "cut a release"

# Keep the defaults, add house rules as the final *static* block
# (cached with the built-ins; dynamic workspace context still follows it)
scode --append-system-prompt "Never push to main." "cut a release"

# Both at once — they compose
scode --system-prompt "..." --append-system-prompt "..." "cut a release"

# Preview what the model will receive
scode system-prompt --append-system-prompt "Never push to main."
```

`--system-prompt` swaps out the static blocks (`You are Sudo Code…`,
`# System`, `# Working`, …); `--append-system-prompt` adds a trailing
block after the workspace context (environment, `AGENTS.md`, auto-memory).
Neither is truncated or escaped, and the workspace context is always kept.
Both are global flags, so they also apply to `scode acp` as the process
default that ACP sessions can further adjust per session via
`_meta.sudocode.systemPrompt` / `appendSystemPrompt` — see
[`acp.md`](./acp.md#per-session-system-prompt-_metasudocode).

## Models

Select a model with `--model`. See [`models.md`](./models.md) for aliases
and provider-specific behavior.

```bash
scode --model opus
scode --model sonnet --auth subscription
```

## Authentication

See [`authentication.md`](./authentication.md).

## Permissions and sandbox

See [`permissions-and-sandbox.md`](./permissions-and-sandbox.md).

## Compacting a long conversation

`/compact` in the REPL, ACP, or `scode --resume <id> /compact` uses the same
model-backed checkpoint pipeline, even below the automatic pressure threshold.
The configured provider must be available. Older history and any previous
checkpoint are summarized together; recent messages and complete tool exchanges
are retained by token budget. Automatic compaction first tries trimming large
tool outputs to avoid an unnecessary model call.

The checkpoint prompt asks for a complete summary within 8,000 tokens where
possible. Generation has a fixed ceiling of 12,000 output tokens, capped by
the model's smaller output limit (including a `maxOutputTokens` override).
The model's context-window limit still applies.

Failed, empty, truncated, or non-shrinking summaries leave history intact and
report an error instead of continuing with a statistical or empty history.
Pre-request failure stops that request. A post-turn maintenance failure keeps
the already completed response. Successful replacements archive the original
JSONL at `<transcript>.before-compact-<timestamp>` before committing the new
history. Keep these files to inspect or recover older context; they are not
subject to automatic cleanup. See [ACP compaction](acp.md#slash-commands) for
budgets and persistence details.

## Images and browser screenshots

The CLI accepts PNG, JPEG, GIF, and WebP references in a user prompt:

```sh
scode --model <vision-model> 'Describe @/absolute/path/screenshot.png'
scode --model <vision-model> 'Describe @"screenshots/home page.png"'
```

The synchronous REPL also submits clipboard images pasted alongside a text
prompt. Image bytes are validated and preflighted before being sent; a missing
or invalid referenced image produces an error rather than a text-only request.
Repeated references to the same spelling of a path attach it once per prompt.

Agents can inspect images using `Read` (`read_file`). For example, after the
independently installed sudohand CLI saves a browser screenshot:

```sh
suh browser page_screenshot --port <browser-port> --path screenshot.png
```

ask scode to **Read `screenshot.png` and inspect the screenshot**. The screenshot
command's path/size response alone does not give the model the image. Read
returns a short textual receipt plus an image attachment, and the next model
request includes the pixels. The image is stored in the transcript so resume
does not depend on the screenshot file still existing. All outstanding tool
replies are sent before image attachments, including parallel Read calls.

Files are read through the filesystem backend and normal tool permissions.
Image source files are limited to 20 MiB; accepted files use the existing
5 MiB / 8000 px image preflight, which downsamples when necessary. Images do
not use text pagination or tool-output truncation.

Vision-capable models receive image attachments directly. If the model
capabilities table explicitly marks the active model as text-only, image Read
returns a tool error asking the agent to switch to a vision-capable model;
a CLI image prompt fails before making a model request. These paths do not
call a second model or use another account. Unknown model capabilities retain
the existing optimistic policy, so the provider may reject image input.

Regression coverage lives in `pty_image_handling` (CLI references, image Read,
parallel results, errors, text-only model rejection, and resume) and the API transport tests.
With suh and Chrome installed, run the actual browser-to-model-payload test:

```sh
cd rust
cargo test -p rusty-sudocode-cli --test pty_image_handling -- --include-ignored
```

This last test scripts only model replies: scode executes Bash, suh captures
real Chrome pixels, and the test verifies those exact bytes in the next model
request. A live model's visual accuracy is a separate check.

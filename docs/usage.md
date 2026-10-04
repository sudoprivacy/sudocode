# Usage

Day-to-day `scode` workflows.

## Global and project directories

All native config paths resolve through `runtime::config`. The same roots own
settings, model configuration, memory, and explicitly installed CLI packages:

| Scope | Directory override | Default |
| --- | --- | --- |
| Global | `SCODE_GLOBAL_CONFIG_DIR` | `~/.nexus/sudocode` |
| Project | `SCODE_PROJECT_CONFIG_DIR` | `<cwd>/.nexus/sudocode` |

`SCODE_GLOBAL_CONFIG_DIR` takes priority over the legacy `SUDO_CODE_CONFIG_HOME`
spelling, which remains supported. Empty config-directory overrides are ignored.
A relative project override resolves against the session working directory.
A relative global override retains the existing process-cwd-relative behavior;
use an absolute path for a global root shared across projects.

Within each root, `settings.json` contains runtime settings and `sudocode.json`
contains model/provider configuration. The global `sudocode.json` supplies the
base; the project file deep-merges on top. Project `settings.local.json` adds
machine-local overrides. Legacy global `scode.json` and project `.scode.json`
remain readable. Existing files keep their names; no duplicate config file or
automatic migration is introduced. The account selection (`auth_profile`)
remains owned by global `settings.json` and project `settings.local.json`.

### CLI packages

Install a package directory, or symlink its **whole package root**, under
`<global config directory>/cli-tools/` or
`<project config directory>/cli-tools/`. Each package must contain `tools/`:

```text
~/.nexus/sudocode/cli-tools/
  feishu-automation -> /path/to/feishu-automation
                       ├── README.md
                       └── tools/
                           └── send_message.py
```

For example, using the default global root:

```bash
mkdir -p ~/.nexus/sudocode/cli-tools
ln -s /absolute/path/to/feishu-automation ~/.nexus/sudocode/cli-tools/feishu-automation
```

At session creation, scode advertises the installed package names and canonical
`tools/` paths in stable name order, plus a fixed instruction to inspect relevant
entrypoints and help. Project installations override global ones with the same
name. Broken links, unreadable entries, names beginning with `.` or `_`, and
packages without a `tools/` directory are skipped. A plain `tools/` elsewhere in
the workspace or an `additionalDirectories` entry is not an installation.

Discovery neither executes package code nor adds model tool schemas. The model
uses its shell tool, retaining each package's launcher, filenames, help, and
working-directory conventions. The catalog is frozen in the session snapshot;
new installations appear in a new session or an explicit feature reload.

### Memory scopes

Native sessions read `<project config directory>/memory/` and
`<global config directory>/memory/`. Put cross-project preferences in global
memory and project-specific facts in project memory. Both use the existing
`MEMORY.md` index and typed Markdown entries. Same-filename precedence is
project, legacy project, then global; every rendered entry includes its source
path so updates and deletions target the right file. All layers share one prompt
budget. Per-agent-type stores use `agent-memory/<type>/` under the same roots.

Existing `~/.scode/projects/<git-root-or-cwd-slug>/memory/` and `agent-memory/`
files remain compatibility sources without being moved or rewritten. New project
facts go in the project config directory. When forgetting a fact, remove its
shadowed copies too so they cannot reappear later.

`SUDOCODE_MEMORY_DIR` remains an explicit, isolated memory root; it does not merge
global or legacy memory. Backend-owned memory roots are isolated the same way.
Disabled memory neither reads nor creates memory directories. Automatic memory-write
permissions respect read-only mode and explicit deny/ask rules, including live
permission-mode switches. Changes to memory files become prompt context in a new
session; compact and resume keep the snapshot.

## Web search with Bocha

Set `web_search` in `~/.nexus/sudocode/sudocode.json`:

```json
{
  "web_search": {
    "provider": "bocha",
    "apiUrl": "https://api.bocha.cn/v1/web-search"
  }
}
```

Export `BOCHA_API_KEY` before starting `scode`, or set `web_search.apiKey`
in your private configuration. `.env` files are not loaded automatically.
`BOCHA_API_KEY` takes precedence over the config key. Bocha uses its own
credentials; it does not reuse the model proxy key.

The API URL above is the default when `provider` is `bocha`; it can be
omitted. `SUDOCODE_BOCHA_API_URL` overrides it, and
`SUDOCODE_WEB_SEARCH_PROVIDER=bocha` selects Bocha for a single process.
The existing `WebSearch` tool returns titles, URLs, and summaries, with
domain inclusion/exclusion, deduplication, and at most eight results.
Existing Tavily configurations continue to use `provider: "tavily"`.

```bash
scode --allowedTools WebSearch "Use WebSearch to find the Rust official website and cite the result URLs."
```

## Interactive REPL

```bash
scode
```

The REPL accepts prose and slash commands. Tab completion expands slash
command names, model aliases, permission modes, and recent session IDs.

`/memory` opens the discovered instruction file in `$VISUAL`, then `$EDITOR`,
or `vi` when neither is set. With several instruction files, type to filter
the file picker, press Enter to edit, or Esc to cancel. With none, it creates
`AGENTS.md` in the current directory. Finish the current turn before editing.
Editor commands accept quoted arguments, for example `EDITOR='code --wait'`;
shell expansions are not evaluated. An existing executable path can be used
directly, including paths containing spaces.

External editors and the `$PAGER` used by long `/status` and `/diff` reports
temporarily receive terminal input and output. Closing them restores the
same REPL and its input state; an editor launch error or unsuccessful exit
also returns to the prompt. A blank `PAGER` disables paging. Scode keeps its
inline display; external programs control their own terminal presentation.

Assistant responses start with a bold text bullet (`•`, U+2022); continuation
lines use a two-column margin. The marker is a text character rather than an
emoji-capable record symbol, avoiding emoji font fallback for this prefix.

The live UI retains the existing formatter colors and supported text styles:
tool-card borders show status without extra icons: amber while running, the
code theme's green on success, and red on failure. The opening and closing
caps stay bold to separate adjacent calls; long vertical borders use normal
weight without dimming. Colors adapt to dark/light backgrounds and terminal
color depth. `ColorTheme` is the single semantic color interface; its `info`
role (including Thinking/Reasoning) and successful tool borders share one
soft-green definition. Syntax-backed colors load on demand and stay cached.
Tool names use the shared Codex blue accent. Titles occupy one
row and use an ellipsis when they exceed the available width. Running titles
reflow on resize; completed titles remain as printed in terminal scrollback.
Bash commands that cannot fit the title are preserved in the card body. Long
list items keep their continuation lines aligned with the item text, including
in session replay. Replayed message blocks have one blank separator.

Queued messages are dimmed, and Todo
items distinguish active and completed states. This uses the same palette as
the transcript; it does not introduce a new color theme. Completed Todo labels
retain their dim strikethrough. Todo summaries use one muted foreground for all
labels and punctuation, with every count bold. Per-turn status uses the same
theme-selected muted foreground without an additional dim attribute; cache
health indicators keep their semantic colors. Emphasis and color are scoped
to individual spans so they do not leak into following labels or input.

Code colors follow Codex's default Catppuccin Mocha (dark) and Latte (light)
themes, including inline code, Bash command previews, language grammars, and
added/removed diff fills. Bash commands are highlighted in both running and
completed cards; stdout/stderr retain the producing program’s own colors.
The default REPL queries the terminal palette once at startup, with a shared
250 ms deadline and preservation of queued keys and pastes. Native Windows
console windows can also supply their color table; ConPTY does not use its
backing console as the visible palette. If the query is unsupported,
`COLORFGBG` selects the theme (dark otherwise). `NO_COLOR` skips the query and
colors; noninteractive commands do not query the terminal. The legacy REPL
uses `COLORFGBG`. Running tool previews cache syntax highlighting; repainting
or editing input does not reparse their code. Output stays in native terminal
scrollback, with no alternate screen. Terminal theme changes take effect on
the next launch.

Markdown uses Codex's typographic hierarchy: headings and emphasis keep the
terminal foreground, inline code and file links use the syntax theme's green,
and web links are blue and underlined. Inline code omits literal backticks;
fenced code has no extra frame or background. Amber remains the UI brand accent.

Resizing the terminal reflows the live UI in place: status, Todo, and queued
message panels remain transient rather than leaving duplicate frames in the
conversation. The current input draft is retained. For UI that fits within
the viewport, resize clears only the live UI from its retained start position,
without clearing the preceding conversation or purging terminal scrollback.

While a turn is running, messages you submit wait in the staging area.
Press ↑ on an empty input to recall the newest queued human message for
editing; its staging entry disappears. Further ↑ presses prepend older
queued human messages, preserving their original submit order. The cursor
stays at the end of the recalled text. Peer/A2A messages remain queued.
Typing, pasting, or pressing ↓ ends this recall sequence and restores
normal cursor navigation. When no human messages remain queued, further
↑ presses preserve the recalled text; submitting it queues the edited
text again. On an empty input with no queued human message, ↑ recalls
prompt history.

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

## Bash tool results

Model-invoked `bash` returns compact JSON: `stdout` and the actual
`exit_code` for a completed process, with nonempty `stderr` when present.
Failures retain `returnCodeInterpretation`; interrupted runs include
`interrupted: true`. Background launches retain `backgroundTaskId` and
`noOutputExpected`, but do not claim a completed exit code. Signal termination
also has no numeric exit code and is described in `returnCodeInterpretation`.

Empty optional fields and routine sandbox capability flags are omitted. The
public Rust result type accepts omitted stderr and interruption fields as an
empty string and `false`, respectively. On failure, an available sandbox
fallback reason is included as `sandboxWarning`.
The full execution struct remains available internally; the compact text is
persisted before it reaches the provider, so resume sends identical results.
Large results still use the existing persisted-output marker and
`read_tool_output` pagination.

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

The shared engine reconciles interrupted tool exchanges before continuing. It
accepts results stored in either `tool` or `user` messages and leaves complete
exchanges unchanged. A call that never returned receives a cancellation result.
An unmatched, late or duplicate result is sent to the model as marked historical
text, including its tool name, ID, output and error state. Its structured source
record remains intact for session replay and `/undo`; recovery never reruns a
tool. The same rule applies to ACP, model changes and subsequent tool round trips,
without retrying a malformed request or discarding the transcript.

## Health check

```bash
scode doctor
```

`scode doctor` reports auth mode resolution, provider reachability, MCP
server status, config resolution, the permission policy, the sandbox
mode, and the tool / skill inventory.

## Prompt cache stability

A session saves its assembled system prompt alongside its transcript. Normal
turns, compaction, process restart, and session forks reuse that snapshot. Editing
`AGENTS.md`, memory files, CLI package installations, skills, or agent definitions on disk does not silently
change an existing session's system prompt. A new session reads the current files.
Plugin/feature reloads explicitly rebuild the snapshot. An explicit new static
prompt override, memory-mode change, workspace/agent change, or changed deferred
tool catalog also builds a replacement. Resuming without repeating a custom
prompt flag keeps the saved prompt.

Snapshots are optional: older transcripts acquire one on their next normal turn
or successful compaction. A failed compaction does not modify the transcript.
The snapshot freezes advertised context, not runtime permission enforcement.
Tool schemas and the deferred catalog use name order. Upgrading from an older
version with a different order can cause one prefix rebuild; subsequent requests
keep that order, with the existing one-time reveal on first tool discovery.

Main-agent turns, subagent turns, and summary requests use the same request
builder. Mock PTY tests capture the HTTP payload after provider conversion and
check system blocks, tool definitions, reasoning settings, routing identity, and
message prefixes. These tests protect the requests scode controls; they do not
simulate actual provider cache hits. Cache usage reported by the real provider
remains the evidence for actual reuse.

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

## Inspecting context usage

`/context` in the REPL draws the context window as a grid of squares, one
color per category (system prompt, system tools, MCP tools, agent types,
memory files, skills, messages), with free space in `⛶` and the buffer
auto-compaction keeps in reserve in `⛝`. The legend lists each category's
estimated tokens and share of the window; the headline total is the
provider-reported occupancy of the latest response once a turn has completed,
and a local estimate before that. The footer summarises each source with a
count and a token total; `/context all` expands it to one line per tool,
agent type, memory file, and skill. Tool definitions are counted exactly as
the next request would carry them: a deferred tool the model has not yet
discovered through ToolSearch is listed but costs no tokens.
The window and auto-compaction reserve use the current session's endpoint
catalog, including discovered limits that override the built-in model table.

## Managing saved sessions

In the iocraft REPL, `/session`, `/session list` or `/resume` opens the same input
component used by `/model`. Type to filter, use arrow keys to select, press
Enter to switch, or Esc to return to the prompt. The current session is marked
in the list. The one-shot `/session list` command still prints a text report.

`/session delete <id>` asks for confirmation in that input component, with
Cancel selected by default. Esc also cancels. `/session delete <id> --force`
skips confirmation; both paths reject deleting the active session. Session
selection and confirmation are available after the current model turn finishes
or is cancelled.

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

## Non-interactive agent tasks (`-p`, `--print`)

Print mode executes one complete agent turn using the normal project context,
model configuration, skills, MCP servers and permission policy, then exits.
Flags may follow the task; use `--` before task text that begins with `-`.

```sh
scode -p "Inspect this project" --model sonnet
cat task.txt | scode -p --output-format json
cat input.csv | scode -p "Summarize these quotes"
scode -p "Continue the review" --resume SESSION_ID
scode -p "Read the configuration" --output-format stream-json --verbose
scode -p -- "--this is task text"
```

Stdin must contain finite UTF-8 input and reach EOF: the first-byte deadline is
3 seconds, the total input deadline is 30 seconds, and the limit is 16 MiB.
A task argument and stdin are combined with an explicit `<stdin>` section.
With no task and no stdin, print mode reports an error; it never starts a REPL.
For inherited pipes with no producer, redirect stdin from `/dev/null` (Windows:
`NUL`). TTY stdin is never read, including for permission or broad-directory
confirmation. Use `--allow-broad-cwd` explicitly when appropriate.

Output contracts (schema version 1):

- `text`: only the last complete assistant answer on stdout; errors on stderr
  retain the CLI's `[error-kind: ...]` classification.
- `json`: one final `type: result` object, including `subtype`, `is_error`,
  `result`, `session_id`, `duration_ms`, `num_turns`, `model_round_trips`,
  `permission_denials`, and observed current-run `usage`. `num_turns` counts
  user turns (one for a started invocation), whereas `model_round_trips` counts
  provider calls, including those following tool results. Unknown usage/cost
  is omitted; usage never includes earlier resumed turns.
  Execution errors include the CLI's machine-readable `kind` alongside `error`.
- `stream-json`: flushed JSON lines: `system` initialization, complete
  `assistant.message.content` (text and tool_use blocks), `user` tool_result
  messages, and one final `result`. Tool inputs are objects; tool results
  reference their call IDs. Assistant IDs are unique across resumed runs.
  This is a documented subset, not the full Claude Agent SDK protocol.

`--verbose` permits diagnostics on stderr. `--include-partial-messages` is
reserved and currently rejected: full tool arguments are not represented as
provider input deltas. Stream output requires `-p`; subcommands and `--compact`
cannot be combined with print mode. Legacy prompt and REPL output is unchanged.

Print mode never grants extra permissions. Requests requiring human approval
are denied immediately and recorded, allowing the agent to recover with an
allowed tool. Questions end the task with `needs_input`, without inventing an
answer. Detached Bash/PowerShell jobs and background agents are refused; agents
must use `run_in_background: false` so their work finishes within the invocation.
Synchronous agents stay synchronous even past the ordinary auto-background
threshold.
On failure or cancellation the engine is closed, with a 10-second cleanup
budget. A stalled stdout consumer has a 30-second write deadline; a broken pipe
cancels the engine. An unwritable pipe cannot receive a final result.

Exit codes: success `0`, execution/startup/output failure `1`, invalid arguments
or input `2`, SIGINT `130`, SIGTERM `143` (Unix). Structured errors use the same
result envelope as success, with `is_error: true` and an error subtype.

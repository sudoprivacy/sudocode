# Usage

Day-to-day `scode` workflows.

## Installation and updates

Choose daemon-free execution or shared local services with
[`scode setup`](shared-services.md). The runtime owner, installation behavior
and separate collaboration-enrollment boundary are defined there.

Install macOS/Linux with the [installer](../install.sh), or choose a platform
package from [GitHub Releases](https://github.com/sudoprivacy/sudocode/releases/latest).
Release builds check for updates in the background when an interactive terminal
session starts. A cached newer release offers **Update now**, **Skip**, or
**Skip until next version** before the session starts. The next launch shows a
newly discovered version. The check runs at most once per 20 hours after
a successful refresh, in the local config home's `cache/release.json`.

```sh
scode update --check
scode update
scode update --version vX.Y.Z
```

Selecting **Update now** installs the checked release and restarts with the same
arguments and working directory. Self-update works on macOS, Linux and Windows;
`scode update` also provides an explicit installation command. Set
`SCODE_CHECK_UPDATES=0` to disable startup checks; `1` enables them in a source
build as well. An unavailable service leaves the terminal usable and retains the
previous metadata.

The default source is GitHub, with COS as the metadata fallback. Choose the China
mirror explicitly with `SCODE_MIRROR`:

```sh
export SCODE_MIRROR=https://sudowork-release-1309794936.cos.ap-beijing.myqcloud.com/sudocode/release/latest
```

An explicit mirror supplies its version pointer, archive and published checksum
manifest. Both the installer and updater resolve `/latest` once, then download
from the immutable sibling `vX.Y.Z` directory. A versioned mirror directory pins
its own version. The publisher verifies the complete mirror before promoting the
pointer; the client verifies the downloaded archive's SHA-256 before installation.

## Persistent peer descriptions

Enable `experimental.peerDescriptions` in settings to let model-backed
compaction publish a short description of each peer's ongoing work:

```json
{"experimental":{"peerDescriptions":true}}
```

The existing compaction call produces optional description metadata alongside
its checkpoint. It adds no separate summarizer or repair request. New agents
have no description until one is published. Tool-output pruning, skipped or
failed compaction, and ordinary turns preserve the previous description and
its timestamp. Child agents retain their existing task descriptions.

The source of truth is `/agents/<name>/description.json` in the actual peer
mailbox namespace. Without a Nexus node, that path is relative to
`~/.nexus/sudocode/local-mailbox/`. The record contains `description`,
`updated_at`, and `source_session_id`. It survives process exit and resume;
resuming an older session does not restore an older description. The owning
agent publishes only after its checkpoint commits. A description changed
during compaction is preserved. Native filesystem locks exclude concurrent
writers; node backends retain their advisory lease guarantee unless they
provide conditional writes.

`agent_list` reads these records and returns optional `description` and
`description_updated_at` fields. Terminal cards show the description and its
age. Missing or unreadable metadata never removes an otherwise reachable peer.
The timestamp records the last description refresh and does not assert that a
peer is running. Descriptions persist until replaced or explicitly removed.

Stored descriptions are not injected into ordinary system prompts or compacted
continuations. Description instructions and a bounded hint of the latest
preserved user intent are appended only to the existing final compaction
instruction. Cached system/tool definitions, historical messages, and the
retained recent tail keep their existing bytes and boundaries. The experiment
is off by default; disabling it preserves the original request and listing
shape.

## Editing a prompt

`Ctrl+U` clears the current draft. You can immediately continue typing or paste
replacement text. Pasting inserts at the cursor and preserves text on either
side. Long or multiline pastes appear as compact placeholders; submitting the
prompt expands them back to the original content, including subsequent typing.

`Up` first moves to the start of the logical line, then walks prompt history;
`Down` moves to its end or restores the saved draft from history. Real newlines,
not terminal wrapping, define these boundaries. Navigation and subsequent
typing share the editor's current buffer and cursor, even in one key burst;
no redraw is required between them. On an empty draft, `Up` recalls queued
human messages before history, leaving inbound peer messages queued.

## Steering, queueing and cancellation

These controls apply to the default asynchronous REPL. Starting with
`SUDOCODE_INTERRUPT_QUEUE_MODE=off` selects the legacy synchronous REPL,
which retains turn-boundary delivery.

While the model or tools are working:

| Key | Behavior |
| --- | --- |
| Enter | Offer the message within the current task. |
| Tab | Queue the message for the next turn; slash command completion keeps Tab. |
| Esc with pending messages | Flush the entire queue into the current task. |
| Esc without pending messages, or Ctrl+C | Cancel the current turn. Ctrl+C always cancels. |

Steering adds ordinary input to the conversation, without an instruction to
abandon the original task. The model can use a suggestion or treat it as FYI.
Sampling yields at replayable text or before a tool is admitted. Consecutive
thinking blocks remain complete and unchanged. Already admitted tools finish
and their results reach the model before the new input.
Flush preserves the unsent draft and does not add an interruption marker.
Cancelling with Esc also keeps the unsent draft; Ctrl+C clears it.

The pending area distinguishes `queued` from `steering`. A steering message
stays visible until it enters the conversation. `Up` recalls explicitly queued
human input, keeping peer messages and accepted steering out of the editor.
Cancellation preserves unapplied messages without automatically restarting
work; Enter or Esc resumes that retained queue. A second Ctrl+C within 800 ms
exits. Session management commands require an idle turn and return immediately
when work is active.

For an interactive peer, `send` accepts `delivery: "steer"` (default) or
`delivery: "queue"`. Peer provenance and message bodies remain intact. Other
hosts deliver at their turn boundaries. Delivery intent never grants a peer
permission to cancel the human's task; the existing structured subagent
shutdown request remains a separate control operation.

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

### Standalone sessions and the local Nexus node

Standalone `scode` reads `~/.nexus/run/node.json` when selecting its peer
namespace. Starting or resuming an ordinary session does not require that node
to answer or this agent's bundle to be ready. A present but unusable record,
missing credential files or an identity mismatch leave A2A unavailable with
the actual cause in `/status` and tool results. They do not redirect messages
to a disk inbox or prevent ordinary model and workspace work.

Background receive failures share one diagnostic reporter across the receiver's
conversations. It retains the first error, changed errors and repeat counts in
the existing rotated local `scode.log` / `sudoclaw.log`; `SCODE_LOG_PATH` can
select the file. A healthy peer does not hide another peer's failure. Retrying
task-completion persistence uses the same reporting implementation. Services
which own terminal notices emit at most one reminder per minute and report
recovery only after all observed failures clear and checks stay healthy.
See [background retry diagnostics](design/background-retry-diagnostics.md)
for the shared implementation and repository audit.

Background receiver errors are available as `Last RX error` in `/status` and
`last_receiver_error` in the structured peer status. This is the most recent
failure in the current process, not a claim that the node remains offline.
It is retained with the receiver's transient state and removed when its last
receiver stops. Hosts can also subscribe to structured tracing. Ordinary
receivers use this status and the local log without emitting terminal notices;
retries continue without writing into the terminal's input area.

The connection record alone does not start Nexus or mint an agent credential.
An explicit [local-owner registration](shared-services.md#recover-an-existing-local-box)
can resume the same locally owned Box after shared selection. A missing record still
selects the same-machine disk namespace for that process. When a node was
selected, its tools and original receiver share one binding: the receiver
retries local configuration and credential resolution, then retries node IO
through the same client. Preparing the missing bundle can therefore make
that binding usable without starting another receiver. A failed or uncertain
send is never automatically resent. Receiver `unavailable` means the receiver
could not initialize; `standby` means another instance holds its seat.

Automatic shared-service preparation keeps ordinary startup usable when that
configured record is unusable; the receiver retains the selected namespace and
reports its cause. Explicit `scode setup` and `setup --check` still return their
actual failures. A dangling node-record symlink is a configured but unusable
record, not permission to prepare a replacement service.

A constructed client is not proof that TLS authentication succeeds. The
client's credentials remain fixed after construction; rotation or replacing
an unusable constructed client is not covered by local binding retries.
Changing a running session from disk to node remains an explicit handoff.
Receiver ownership and a selected node endpoint in `/status` are not proof
that a remote peer is online. Cohost continues using its injected mailbox and
requires its host services.

An ordinary HTTP model can run while A2A is unavailable. A model explicitly
configured through a `nexus://` mount still needs the selected node's
authenticated filesystem; model traffic keeps that mount's authorization and
hooks.

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

The live status follows the foreground engine lifecycle. A four-cell amber
animation approximates the rotating Sudoprivacy logo in one row, using twelve
precomputed frames. Model work shows `Thinking...` or `Reasoning...`.
The activity label uses the theme's info/warning color; model, elapsed time and
tokens use its muted color so they remain secondary to the activity.
Elapsed time and ETA refresh in whole seconds; tokens and the brand animation
remain live. Final turn statistics retain their precise duration.
Foreground tools use the same row and show available progress first. Quiet
tools show `Running Bash…` or `Running 2 tools…`. Long descriptions truncate
before the time/token details.
Questions and approvals stop the animation and use the existing input panel;
there is no additional waiting row. After the turn ends, the normal result and
footer return. Background work keeps its separate task summary in the footer.

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

Unfinished ordinary paragraphs and top-level fenced code stream above the live status area by default.
Finalized text
moves into native scrollback in the same presentation as preview removal,
and a draft typed during the response stays available. Parallel tool output
keeps unfinished Markdown formatting intact. The preview keeps
at most 8 KiB of source and fits around the input; longer paragraphs freeze
their preview until finalization, which still retains the entire response.
Complete code lines retain syntax parser state across deltas; the unfinished
last line stays literal until its newline arrives. Finalization uses the same
highlighter as replay. Nested/indented fences, Mermaid diagrams, tables, lists, headings and emoji with ambiguous terminal widths use
the existing complete-block rendering path. On Windows, this also applies to
combining sequences whose widths legacy console hosts cannot reliably preserve;
the original Unicode text is retained without normalization. A clipped paragraph or an
unusually long result summary can still change its visible position.

Completed `mermaid` fences render as terminal diagrams through the same Markdown
path for streamed answers, replay and Markdown prompts. The renderer reuses
Codex's pinned native engine; no browser, JavaScript runtime or external process
is needed. Diagram borders use the existing theme and `NO_COLOR` remains supported.

Use compact flowcharts, sequence diagrams, flat state diagrams, class diagrams
or ER diagrams. Prefer `flowchart TD` on narrow terminals. See the upstream
[supported syntax and limits](../rust/vendor/codex-mermaid/README.md): this is a
bounded subset rather than full Mermaid support. Unfinished fences stay source;
unsupported syntax or diagrams that cannot fit show a reason and the original
source. Control characters in source fallback display as escapes. The persisted
Markdown stays unchanged, and replay renders against its current terminal width;
diagrams already committed to native scrollback do not reflow on resize.

The default system prompt advertises when a diagram helps and how to stay within
this subset. Explicit system-prompt replacement still replaces those defaults.

The live UI retains the existing formatter colors and supported text styles:
tool-card borders show status without extra icons: muted while waiting for
hooks, approval, or an execution slot; amber while running; the code theme's
green on success; and red on failure. The opening and closing
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

In the live REPL UI, Bash and MCP progress takes priority in the existing
status row; input requests take priority over activity. It does not append partial
output or spinner summaries to terminal history. The completed tool card
retains its normal output preview. Progress
is laid out with the same column and grapheme rules as other terminal text;
external cursor/screen commands cannot take ownership of the live region.
Paths without a live UI retain bounded progress log lines. Buffered answer and
reasoning text closes at model content transitions and message/turn boundaries,
without filtering literal words from the model's response. Asynchronous tool
results, notices and interactive prompts do not terminate an unfinished Markdown
block; completed results remain visible while the provider is still streaming.
Captured diagnostics use the same line framing and preserve UTF-8 characters
split across reads.

Independent reads, searches, sub-agent calls, and recognized read-only Bash
commands can execute concurrently, with a default limit of ten. Each complete
tool input becomes eligible while the provider is still streaming. Pre/post
tool hooks run asynchronously; a pre-hook rewrite is classified again before
execution. Writes and unrecognized Bash commands fence later calls until they
finish. Recognized Bash forms include read-only `git branch`, `fd`, `diff`,
selected `gh` queries, literal globs, and file input redirects; unknown or
mutating flags remain serial.

`AskUserQuestion` can wait alongside independent reads. Questions and permission
requests share one input queue, so only one prompt owns the editor at a time;
other results continue to render while it waits. MCP tools
can overlap when they declare `readOnlyHint` and use stdio or Streamable HTTP;
other MCP transports and resource/prompt wrappers remain serial. A visible
tool card may still be queued: the amber border marks actual execution.
Completed results appear immediately, even while another call is still running.
Cancellation or a broken provider stream stops unfinished calls and preserves
finished outputs. An interrupted provider response is not automatically replayed
once tools have started. Streamed calls are saved before dispatch; append-log
`assistant_update` records extend the current assistant message without rewriting
old history. Normal snapshots fold them into ordinary messages. Reading an
append log that still contains these records requires a version that supports
streamed tool execution; this includes native append streams retaining earlier
snapshots. Ordinary rewritten transcript snapshots remain compatible.

A second `message_start` in the same upstream response is a protocol error.
Sudo Code preserves received partial progress and stops without replaying the
request or combining usage from the two responses. HTTP failures before a
response starts remain eligible for the normal transport retry policy.

Incoming peer messages appear in a separate `Message from <peer>` frame with
muted framing and a blue peer name. Sender-supplied `Summary:` text appears above
the complete body and is preferred in the one-line queued preview. Live display
and resumed history use the same presentation; older messages without a summary
continue to show their body. The summary travels with the existing envelope into
the saved turn and model context.

Outgoing `send` cards keep the summary concise and preview up to six source
lines of the body, with a character cap. Lines wrap to the terminal width; a
truncation notice points to the complete message preserved in the session.

Queued messages use muted labels and normal-intensity bodies, and Todo
items distinguish active and completed states. This uses the same palette as
the transcript; it does not introduce a new color theme. Completed Todo labels
retain their dim strikethrough. Todo summaries use one muted foreground for all
labels and punctuation, with every count in bold terminal foreground, including
zero. Ordinary task descriptions also retain the terminal foreground instead
of a slot-wide grey; active labels remain bold and completed labels retain dim
strikethrough. Amber is reserved for accent/activity, not neutral quantities.
Per-turn status uses the same
theme-selected muted foreground without an additional dim attribute; cache
health indicators keep their semantic colors. Emphasis and color are scoped
to individual spans so they do not leak into following labels or input.

Ordinary footer tips use the theme's readable muted foreground, actionable
recommendations use its primary accent, and urgent tips use its warning role,
without an additional dim attribute; separators use the border role. The input draft
retains the terminal foreground. A slot's initial color is decoded as text
state, not applied again as a fallback to every span: ANSI foreground/full
resets restore the terminal default instead of silently inheriting a parent
color. The same structured spans feed the live UI and transcript adapters.
No-color chrome does not force a palette color.

Code colors follow Codex's default Catppuccin Mocha (dark) and Latte (light)
themes, including inline code, Bash command previews, language grammars, and
added/removed diff fills. Bash commands are highlighted in both running and
completed cards; stdout/stderr retain the producing program’s own colors.
Recognized plain unified diffs reuse the Edit palette; ordinary logs stay plain.
Write and Edit share numbered, syntax-highlighted source previews. New files
show added lines; replacements show removed/added lines with nearby context.
Running Write previews show the submitted content; completed cards use the
tool's returned content. Preview lines and characters are bounded before
highlighting, with explicit omission notices. The complete source is preserved
in the session and file. Markdown files keep literal Markdown source in these
tool cards rather than becoming rendered documents.
When a large tool result is offloaded, the card labels its retained request
content as an input preview instead of claiming an empty file or guessing
whether the operation created or replaced it. Replay uses the recorded request;
the complete tool result remains available through the session's output record.
On Unix, the default REPL queries the terminal palette once at startup, with
a shared 250 ms deadline and preservation of queued keys and pastes. Windows
does not send OSC 10/11 color queries: some console hosts deliver replies as
input, so startup uses `COLORFGBG` instead (dark if absent or invalid). This
also avoids treating ConPTY's backing-console colors as the visible theme.
Unsupported Unix queries use the same fallback. `NO_COLOR` skips the query and
colors; noninteractive commands do not query the terminal. The legacy REPL
uses `COLORFGBG`. Running tool previews cache syntax highlighting; repainting
or editing input does not reparse their code. Output stays in native terminal
scrollback, with no alternate screen. Terminal theme changes take effect on
the next launch.

Markdown uses Codex's typographic hierarchy: headings and emphasis keep the
terminal foreground, inline code and file links use the syntax theme's green,
and web links are blue and underlined. Inline code omits literal backticks;
fenced code has no extra frame or background. Amber remains the UI brand accent.
Tables follow Codex's column allocation: paths and long tokens surrender width
before prose and compact values. Cells wrap with their styles and links;
tables that cannot remain readable become labeled key/value records. Layout
uses the available width when rendering, including replay and plan review.
Already printed native scrollback keeps the terminal's own resize/reflow behavior.

Markdown web links use terminal-native OSC 8 hyperlinks. Supported terminals
show the label (for example `[PR #891](https://github.com/sudoprivacy/sudocode/pull/891)`);
unknown terminals and multiplexers also retain the destination. Pipes and dumb
terminals keep readable text. `NO_COLOR` removes color and text styling while
preserving supported links. Only bounded HTTP(S) destinations become hyperlinks;
file references keep their existing path display. The terminal owns click gestures,
selection and copying: ordinary selection copies visible text, not the hidden URL.

Links share the structured text and layout implementation across scrollback,
resumed history and interactive Markdown prompts. They do not add model tools,
network lookups, system-prompt instructions or mouse capture.

Plan approval and model question descriptions use the same Markdown renderer
as assistant answers and transcript replay. Both choice widgets and the legacy
REPL preserve headings, lists, code, tables, and links. Plain configuration and
permission descriptions keep their literal text. The full description is
written once to native terminal history when review opens. Use the terminal's
normal scrollback to read it; the input slot contains the current choices and
feedback. Moving between fields, typing and status updates do not reprint the
document or move its content into a clipped live panel.
During review, `Ctrl+O` opens the same frozen description through `$PAGER`,
including content beyond the terminal's scrollback limit. Closing the pager
restores the pending choice and feedback without answering the question.

Live slots share one row budget. InputSlot has priority: when a long draft or
small window leaves insufficient space, PendingSlot, StatusSlot, TodoSlot and
FooterSlot fold into explicit summaries rather than silently disappearing.
Growing the window restores their full contents. Long drafts scroll within the
same editor, preserving the complete text and cursor.

The choice controls stay visible while the document is read in native history
or the pager. If the terminal is too small to show those controls
safely, an enlargement warning replaces the panel; confirmation and hidden
edits are disabled. Ctrl+C can still cancel; Esc follows the pending-message
controls described above. Resizing does not answer a question, alter the saved
plan, or clear conversation history.

`write_plan` saves the complete Markdown document, including Context,
Constraints, and Acceptance Criteria, before presenting it for review. The
saved document is the source for both the preview and the approved tool result.
Both REPLs show it in a distinct `Plan · awaiting review` frame with the saved
file path. Reasoning stays above the review, and the choices stay below it.
Completed `WritePlan` cards show the review decision and file path rather than
abbreviated plan arguments. Revision feedback and cancellation are normal
review outcomes; failures to save or validate a plan remain errors.
On resume, review documents are reconstructed from their recorded tool inputs
with the same document composer. A later edit to the current plan file does
not rewrite an earlier proposal in conversation history.
If it changes while review is pending, it must be presented again for approval.
“Clear context & execute” discards exploration messages after the current tool
exchange completes, then continues execution in the same turn without another
user prompt. “Keep context & execute” continues with the existing history.
Clearing preserves the session identity, todos, model, and frozen system/tool
prefix; the previous history is archived through the normal compaction path.
Comments and rejection retain the draft. A noninteractive caller saves a draft
without manufacturing user approval.

Resizing the terminal reflows the live UI in place: status, Todo, and queued
message panels remain transient rather than leaving duplicate frames in the
conversation. The current input draft is retained. For UI that fits within
the viewport, resize clears only the live UI from its retained start position,
without clearing the preceding conversation or purging terminal scrollback.

While a turn is running, Enter steers the current task and Tab queues a message
for the next turn. Pending messages stay visible in the staging area until
consumed, with `queued` and `steering` labels indicating their delivery state.
Each queued preview uses a muted label and normal-intensity message text;
narrow windows shorten the label and clip the preview, not the queued text.
Press ↑ on an empty input to bring all queued human messages back for editing,
joined in their original submit order. Their staging entries disappear;
peer/A2A messages remain queued. The cursor starts at the end of the recalled
text, and further arrow presses use normal cursor navigation. Enter steers with
the edited text; Tab queues it again. On an empty input with no queued human message,
↑ recalls prompt history.

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
`rawOutputPath`, but do not claim a completed exit code. Signal termination
also has no numeric exit code and is described in `returnCodeInterpretation`.

Windows console-control exits are treated as interruptions even if the child
exits before the parent's cancellation notification arrives. Partial output is
preserved; an interrupted tool does not become a successful result.

Empty optional fields and routine sandbox capability flags are omitted. The
public Rust result type accepts omitted stderr and interruption fields as an
empty string and `false`, respectively. On failure, an available sandbox
fallback reason is included as `sandboxWarning`.
The full execution struct remains available internally; the compact text is
persisted before it reaches the provider, so resume sends identical results.
Large results still use the existing persisted-output marker and
`read_tool_output` pagination.

## Contextual footer tips

The row below the input combines permission mode, background task counts and
one contextual tip. General startup guidance also lives here. The highest
priority applicable tip wins: immediate local interaction feedback, then model
tips ordered `urgent` > `important` > `normal`, with local cache advice at
`important` priority. At equal priority an explicit model tip takes precedence
over cache advice. Routine `/help` guidance fills otherwise unused space,
led by the workspace's position: `⎇ <branch>`, or `⎇ <short commit>` when HEAD
is detached. The position is state rather than advice, so every tip takes the
slot from it; it is read at startup and refreshed when each turn ends, and the
per-turn status line no longer repeats it.
While a turn runs, only urgent tips are shown.

The model has an always-loaded, read-only `set_tip` tool:

```json
{"priority":"important","message":"Check the migration before deploying."}
```

Both fields are required. Use `urgent` for immediate user action, `important`
for actionable advice, and `normal` for optional guidance. Each priority has
one replaceable model tip; an empty message clears that priority. Messages
must be one plain-text line, at most 160 Unicode characters, without control
characters. Invalid calls return ordinary tool errors. Tips request neither
approval nor user answers and do not change configuration. Model tips clear
when the next turn starts or the context changes. Saved tool traces are not
replayed as active footer advice.

Tips travel through the existing tool-result stream. The interactive REPL
shows successful tips in the footer; the saved tool trace, headless output
and ACP tool results retain their structured data. Question and permission
instructions stay with their controls. The footer remains one row on resize;
it compresses background counts or yields their space for important tips,
and truncates text on Unicode display-column boundaries. Theme colors apply
to individual spans and no-color terminals retain their default foreground.

With `experimental.cacheTtlAdvisor` enabled, a changed TTL recommendation is
shown after a committed compact and on resume. It reads the same frozen
decision as `scode cache stats`, uses no extra model call, and states that
the recommendation has not been applied. The advisor remains off by default.

## Background tasks in the REPL

Background Bash/PowerShell terminals and this session's background sub-agents
share a compact entry on the existing footer row. Explicit background launches
and agents automatically moved to the background use the same view. It shows
running terminal/agent counts and unread completion results. When neither
exists, the entry is hidden; no additional permanent row is reserved.

The task summary normally shows just the counts. With an empty prompt, press
**↓** to focus it; **Enter to view** then appears. Press **Enter** to open the task list.
**↑/↓** selects a task, **Enter** opens its details, and **k** stops the selected
running task. In details, **↑/↓** or **PageUp/PageDown** scrolls the output tail;
**Esc** returns to the list, then **Esc** returns to your preserved prompt.
`/tasks` (or `/ps`) also opens the browser, including completed history after
the footer entry disappears. Existing `/tasks list|get|stop` commands remain
available for persisted agent manifests.

Shell details update while the parent is idle and include stdout/stderr,
elapsed time, status, and the actual exit code. The preview retains at most
24 KiB per task; complete shell output is stored in the private temporary log
shown by **Full output** and `rawOutputPath`. Logs remain in the OS temporary
directory after completion. The session retains at most 50 completed tasks
in memory. Agent details show current activity and recent output, then their
final result. These are tasks owned by this session, not a view of other agents
on the machine.

Reading a completed task acknowledges its result and removes its unread
indicator. Inspecting a running task does not acknowledge a future result;
completion while its details are open is already visible. Closing the browser
does not cancel work. Questions and permission prompts retain keyboard
priority. Stopping a task cancels its process group and owned child tasks;
exiting the session stops and reaps background processes. Background work
survives cancellation of an unrelated parent turn. Sub-agent completion keeps
using the existing automatic notification/queued follow-up path.

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

`latest` prefers your most recently used interactive session with saved history. A newer
`--print`/`-p` run keeps its own transcript without displacing that session.
One-shot transcripts remain resumable by ID or path, and are the fallback
when no interactive history exists. Empty startup sessions stay behind saved
conversations. Resuming a one-shot transcript in the REPL makes it an
interactive session for subsequent `latest` selection. Older transcripts
without a session-mode field remain interactive for compatibility.

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
estimated tokens and share of the window. The headline total uses a valid
provider-reported occupancy anchor from the latest task response. Without one,
it shows a full next-request estimate, including the actual system and tool
overhead, marked `≈`. Successful compaction clears the old anchor; manual,
automatic, and resumed sessions refresh the meter without resetting cumulative
tokens or cost. Summary-request usage counts toward the cumulative bill but
does not become a task-response anchor or add a conversation turn.
The footer summarises each source with a count and a token total;
`/context all` expands it to one line per tool,
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
Older history and any previous checkpoint are summarized together. The whole
replacement history, including the checkpoint, recent messages, continuation
framing, and Todo state, must shrink to at most **50% of the original history**
and fit the next request's safe budget. **30% is the ideal target**, rather than
a minimum or a separate success condition. These figures use local content
estimates; reaching them does not guarantee lossless summarization.

The retention ratio uses the original history before any tool-output trimming.
The run retains at least four recent messages and complete tool exchanges by
token budget. Automatic compaction first stages large-tool-output trimming; it
can commit that alone only when the same 50% and request-budget checks pass.
Otherwise, both summary attempts reuse that same frozen, trimmed copy; manual
compaction summarizes its frozen original source. If protected content prevents
the target, the operation fails without installing a weaker replacement.

The visible summary allowance is dynamic: at most 12,000 tokens, reduced by the
space left after retained history and framing, the next-request target, and
the model's output limit (including `maxOutputTokens` overrides). The prompt
keeps 8,000 tokens as soft guidance and asks for a shorter summary when needed.
Budgeted thinking can require additional output reservation. Adaptive thinking
does not add a manual thinking budget to the summary's output ceiling. Both the
summary request and the following task request are checked against their own
actual system, tool schemas, output reservation, and safety buffer; pending
user input also reduces the next request's history budget.

One run allows at most two completed summary responses, four actual model HTTP
requests, and two transient retries, shared across cache-safe generation,
standard-path fallback, and shorter-summary attempts. Local preflight rejection
does not count as an HTTP request. Authentication and permanent protocol errors
stop immediately; unchanged oversized requests are not resent.

Reports distinguish `target_met`, `skipped`, `failed`, and `cancelled`, with
the method, estimated before/after history, retention ratio, target, and reason.
An unchanged, previously committed achievement with the same request state and
budget can skip without another model call; history with no summarizable source
can also skip when the original request is safe. A skipped report describes the
current unchanged history and does not claim a new reduction. The configured
provider must be available when a summary request is needed.

Empty, truncated, tool-calling, non-shrinking, or over-target summaries are never
installed. Failure or cancellation before commit preserves source messages;
pre-request failure stops that request, while post-turn maintenance failure
keeps the already streamed response in history but still fails the prompt/run.
Streamed output alone does not make that run successful. Cancellation after a
durable commit reports the committed result. Resumed one-shot `/compact` handles
Ctrl-C by waiting for its final report and usage receipts before exiting.

Known usage from every summary attempt, including rejected summaries and
cancelled requests, is saved separately as maintenance usage and counted once
toward cumulative billing. Unknown usage stays unknown in its receipt, leaving
the complete cumulative paid amount unknown while retaining known token
subtotals. The status line can still show an estimated cost marked `~`.
Maintenance metadata does not change active history or its occupancy anchor;
a persistence failure is reported explicitly. Durable maintenance receipts are
merged when capturing the source revision, including receipts added externally
while message history stays unchanged.

Before archiving or replacing history, the source's durable revision is checked.
A detected external change rejects the candidate and reloads the newer source
without writing the stale snapshot; unsaved maintenance metadata is reported.
If the source cannot be parsed or reloaded, ordinary persistence and exit do
not write back the stale history. A successful reload restores normal writes.
The check detects changes observed during commit preparation and is not a
cross-process lock or compare-and-swap. Successful replacements archive the
original JSONL at `<transcript>.before-compact-<timestamp>` before committing the new
history. Keep these files to inspect or recover older context; they are not
subject to automatic cleanup. See [ACP compaction](acp.md#slash-commands) for
budgets and persistence details.

### Limit execution steps per turn

Set `maxSteps` in `.nexus/sudocode/settings.json` (or user settings):

```json
{"maxSteps": 5}
```

Use a positive integer, for example 5 or 10. Omit the setting for unlimited
execution. One step is one completed model/tool round; several tool calls in
one model response count as one step. After the last round's results arrive,
scode sends a final request with no tools and instructs the model to answer
using existing results, describe unfinished work, and discuss concrete next
steps with the user. This final answer is an additional model request, outside
the execution budget. Each new user turn gets a fresh budget. Already-started
background work is not cancelled by this limit.

## Editing files

`edit_file` requires a nonempty `old_string`. By default it must match exactly
once, including after LF-to-CRLF normalization. Ambiguous edits return an error
without modifying the file. Include more surrounding context to select one
occurrence, or set `replace_all: true` to replace every non-overlapping match.
Use `write_file` to create a file or replace its full contents.

## Images and browser screenshots

The CLI accepts PNG, JPEG, GIF, and WebP references in a user prompt:

```sh
scode --model <vision-model> 'Describe @/absolute/path/screenshot.png'
scode --model <vision-model> 'Describe @"screenshots/home page.png"'
```

In the REPL, paste a clipboard image with **Alt+V**, with **Ctrl+V** where the
terminal passes it through, or with the terminal's own paste (Cmd+V on macOS)
when the clipboard holds only an image. On Windows Terminal, Ctrl+V is the
terminal's text paste, so use Alt+V there. The image shows as `[Image #N]` in
the default REPL and as its `@"path"` reference in the classic synchronous REPL,
and it is attached on submit exactly like a typed reference: a copied image file
is referenced where it is, and pasted image data is stored in the image cache
first. For scripts and tests, `SUDOCODE_CLIPBOARD_IMAGE=<file>` stands in for
the system clipboard, which is then never read: the clipboard holds only the
image in that file, or no image when the file is not one.

Image bytes are validated and preflighted before being sent; a missing or
invalid referenced image produces an error rather than a text-only request.
Repeated references to the same spelling of a path attach it once per prompt.

Agents can inspect images using `Read` (`read_file`). For example, after the
browser skill uses the installed `ai-dev-browser` package to save a screenshot:

```sh
python3 -m ai_dev_browser.tools.page_screenshot --session-id <browser-session-id> --path screenshot.png
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
With `ai-dev-browser` and Chrome installed, run the browser-to-model-payload test.
Set `SCODE_BROWSER_PYTHON` to the interpreter containing the package if it is
installed in a virtual environment:

```sh
cd rust
cargo test -p rusty-sudocode-cli --test pty_image_handling -- --include-ignored
```

This last test scripts only model replies: scode executes Bash, ai-dev-browser captures
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
  is omitted; usage never includes earlier resumed turns. Usage includes
  `total_tokens` and `token_breakdown_complete`; `reported_total_tokens` is
  included when the provider supplies an aggregate. When allocation is
  incomplete, the total survives and the breakdown is marked incomplete.
  Missing cost fields stay omitted rather than becoming a zero bill.
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

## Runtime memory

Performance reports measure peak resident set size (RSS), RSS after the workload,
and growth across turns for the release `scode` process. Relevant PRs use paired
baseline and candidate runs; the daily stress profile adds 30 turns. The
[performance CI](https://github.com/sudoprivacy/sudocode/actions/workflows/render-performance.yml)
retains the raw reports, and the
[measurement guide](../e2e/terminal-resize/README.md#release-performance-gate)
defines the workloads and regression checks.

The [2026-10-09 UTC stress run](https://github.com/sudoprivacy/sudocode/actions/runs/37995377154)
measured **28.27–32.71 MiB peak RSS** across its scenarios, using the median of
seven candidate runs per scenario. Its 30-turn scenario peaked at **29.42 MiB**,
with **1.77 MiB** growth from the first to the last turn. The run used Linux and
a deterministic provider. Scope is one `scode` process; child tools, browsers,
Nexus, and the model server are measured separately when assessing a complete
deployment. Apple/Spark measurements require workloads on that hardware.
RSS includes shared pages; unique memory across several processes requires a
measure such as proportional set size (PSS).


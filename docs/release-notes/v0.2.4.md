# Sudo Code v0.2.4

## What's New

Internally this release finishes the engine/renderer split: `EngineEvent` /
`EngineCommand` are now the only types crossing between the engine and anything
that renders it, the ACP server consumes that seam rather than its own delegate,
and CI enforces the boundary. That work is invisible from the outside — but
getting the Windows test suite to run against it uncovered a run of real bugs,
and those are what this release is worth installing for.

## Fixed

**`--allowedTools` refused the tool names models actually use.** The allow-list
is stored canonicalised and tool dispatch canonicalises before matching, but the
gate compared the raw name — so a model calling `Bash` was rejected under
`--allowedTools bash`. Every tool call in an allow-listed run was refused, and
because the model then explained itself politely instead of failing, it read as
the model declining to work. Affects every platform.

**Streaming bash captured no output on Windows.** The two pipe readers were
`tokio::select!` branches, and `read_line` is not cancellation-safe: each time
one resolved, the other's in-flight read was dropped. On Unix the bytes stay in
the pipe; on Windows `tokio::process` reads through the blocking pool, so they
were lost. `printf 'hi'` returned exit 0 and empty stdout — silently, on the
path the REPL uses.

**ACP answered `session/prompt` before sending that turn's updates.** A client
renders `session/update`s as they arrive and finalises on the response, so an
update behind it is one the client has stopped listening for. Slash commands
lost their output entirely over WebSocket.

**ACP could hang after a turn.** A per-turn hook-progress reporter was installed
from the observer and never cleared, pinning the renderer's event channel open
after the turn had returned.

**`scode acp` never exited on Windows when its host disconnected.** The
stdin-EOF watchdog was a `poll(2)` probe and therefore Unix-only, leaving one
orphaned process per session. Replaced on all platforms by an EOF-aware stdin
wrapper around the transport's own reader.

**One-shot turns could not be cancelled from a new process group.** Windows
disables Ctrl-C for a group created with `CREATE_NEW_PROCESS_GROUP` — the
standard way a job runner isolates cancellation — leaving Ctrl-Break as the only
signal that reaches the child. It is now handled as the same cancel.

**Memory entries were dropped over frontmatter case.** A file that fails to
parse is skipped silently, and a model writing `TYPE: FEEDBACK` for
`type: feedback` produced exactly that. Field names and type values are now read
case-insensitively.

## Testing

Every `cfg(unix)` gate and Windows-only skip is gone from the CLI test suite:
the full PTY suite, ACP integration included, now runs on Windows in both mock
and live modes. The suite also no longer reads or writes the developer's real
`~/.nexus/sudocode` — each test gets a throwaway config home, and the live
account is chosen per run via `SCODE_LIVE_AUTH_PROFILE`.

# Sudo Code v0.2.5

## What's New

This release is about the model you actually run on, and about paste. Bracketed
paste now works on Windows and behaves like Claude Code; the model default is
resolved from one place so changing it actually sticks; and a class of dropped
streams no longer surfaces as a spurious error.

## Fixed

**Bracketed paste on Windows, and a REPL freeze on paste + Enter.** Pasting
multi-line text into the REPL now shows a compact `[Pasted text #N +M lines]`
placeholder instead of submitting one turn per line, on Windows as well as
Unix. Windows support comes from the sudoprivacy `crossterm` fork (upstream
`crossterm-rs/crossterm#1030`): the legacy Win32 console never produced
`Event::Paste`, so it enables VT console input for the lifetime of bracketed
paste and feeds the byte stream through the shared parser. Two paste bugs came
out with it: pasting content that itself contained a `[Pasted text #N]` string
and pressing Enter looped the placeholder-expansion forever and froze the whole
REPL (keyboard and Ctrl-C dead); and Windows clipboards deliver CR/CRLF, which
the line counter ignored, so multi-line pastes showed no `+N lines`. Both
fixed, with the trigger now matching Claude Code: only long (> 800 chars) or
multi-line (> 2 lines) pastes collapse into a placeholder; shorter pastes insert
literally.

**The model default is resolved from config, everywhere.** Two independent
issues made a resumed or `auto` session ignore your configured model. `--resume`
pinned the model recorded in the transcript, so changing the global default did
not apply on resume; it now reads the config default (an explicit `--model`
still wins, and `/model` switches remain runtime-only). And the `auto` alias was
hardcoded to `claude-sonnet-4-6`, silently overriding any configured default for
anyone on `model: auto`; `auto` now resolves to the compiled-in default. Both
paths agree on one source of truth, so auto-compaction sizes the context window
correctly regardless of what a transcript recorded.

**A dropped stream at the very end no longer forces a retry.** When a proxy cut
the connection right after Anthropic's terminal `message_stop` event, only the
closing bytes of the final frame were lost, but the parser reported a retryable
`IncompleteStream` -- forcing a wasteful whole-turn retry, or an infinite loop
against a proxy that kept truncating the same terminal frame. A truncated
`message_stop` is now treated as the complete message it is; a mid-content
truncation still errors and stays retryable.

## Testing

The bracketed-paste behavior is covered by human-fidelity PTY tests that drive
`scode` through a real PTY (Unix pty and Windows ConPTY) and assert on the
rendered screen: multi-line placeholder counts for LF/CRLF/CR, short paste stays
literal, and paste-containing-placeholder + Enter does not freeze. The model and
SSE fixes carry focused unit tests, including the resume resolver and the `auto`
alias.

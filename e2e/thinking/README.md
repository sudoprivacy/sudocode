# Live signed-thinking acceptance (Windows)

This runner drives a real `scode` process through Windows ConPTY and a real
Anthropic-compatible HTTPS route. It reads fresh random values with Read,
calculates their sum, exits, removes the input file, resumes the saved session,
and subtracts 17 using the persisted result.

It checks the actual tool result and both outgoing assistant histories against
the signed thinking block received in the original raw SSE. Missing,
changed or lost signatures fail even if a gateway tolerates the broken history
and returns a correct answer. Final answers must contain only the computed
result, so repeated planning prose also fails. New standalone `course`, `court`
or `card` output fails. A pass does not establish why an older polluted session first
generated those words.

```powershell
python -m pip install -r e2e/thinking/requirements.txt
python e2e/thinking/run.py C:/path/to/scode.exe --profile sudorouter --model claude-sonnet-4-6 --artifacts C:/path/to/artifacts
# Select high effort for a comparison with Claude Code's recorded default:
python e2e/thinking/run.py C:/path/to/scode.exe --profile sudorouter --model claude-sonnet-4-6 --effort high --artifacts C:/path/to/artifacts
# Also require the real provider to retain project instructions:
python e2e/thinking/run.py C:/path/to/scode.exe --profile sudorouter --model claude-sonnet-4-6 --effort high --check-context --artifacts C:/path/to/artifacts
# Require a nonempty thinking summary and preserve its text and signature:
python e2e/thinking/run.py C:/path/to/scode.exe --profile sudorouter --model claude-sonnet-4-6 --thinking-display summarized --effort high --check-context --artifacts C:/path/to/artifacts
# Verify Opus 4.8's native thinking configuration through the same real journey:
python e2e/thinking/run.py C:/path/to/scode.exe --model claude-opus-4-8 --preserve-thinking --thinking-display summarized --effort high --check-context --artifacts C:/path/to/artifacts
```

Credentials come from the named proxy profile in
`~/.nexus/sudocode/sudocode.json`; use `--config` for another file. The child
process has an isolated home/config and only a placeholder key. Real keys stay
in the recording proxy's memory and are excluded from saved requests.

The proxy sets `thinking: {type: adaptive, display: omitted}` by default.
`--thinking-display summarized` selects a separate profile that requires a
nonempty signed summary; it does not count as empty-block coverage. Both profiles
compare the returned thinking text and signature exactly against the tool
continuation and resumed request. The selected `--effort` is `low` by default,
or `high`. The startup output records both controls. It caps each response at
2048 tokens and permits at most five message requests. The expected journey
uses three requests. This validates signed-block preservation with that
explicit profile; it does not test the CLI's default thinking configuration.

`--preserve-thinking` instead passes the selected effort to the CLI, checks its
actual thinking and effort fields, and forwards those fields unchanged. This
detects the Opus 4.8 regression where the CLI sent an unsupported manual budget.
The 2048-token response cap and five-request limit still apply. Normal runs use
gateway routing; `--upstream-channel` is only for a controlled route comparison.

`--check-context` adds a fresh random nonce to the fixture's `AGENTS.md`,
checks that the CLI sends it in system context, and requires its exact value
alongside both computed answers. The nonce is absent from the input file and
first user prompt. A correct sum alone cannot pass this check. Missing or
replaced project context fails even when the route returns HTTP 200 and the
Read tool succeeds.

The startup output records the session UUID. `--session-id <UUID>` reuses a
diagnostic UUID and checks that the CLI forwards it in `metadata.user_id`.
This holds the requested session identity constant when comparing fresh
workspaces or request profiles. Each workspace still has new file values and a new context
nonce, so a cached old answer cannot pass.

The provider must support the selected profile and return its required signed
block shape; otherwise the test fails instead of counting another response shape
as coverage. The runner waits for the complete captured response before exiting
each process, so final text arriving before `message_stop` cannot leave the
last response unrecorded. Workspaces are created beneath `%PUBLIC%` and must have no inherited
agent instructions. Processes, the local recording server and workspace are
cleaned up on completion or failure. `--artifacts` retains individual request
bodies, raw responses and terminal screens in a fresh directory.

The portable CI regression for block boundaries and resume lives in
`rust/crates/rusty-sudocode-cli/tests/pty_cache_prefix.rs`:
`thinking_blocks_survive_turns_and_resume_without_merging_or_dropping`.
This runner is a funded manual acceptance test and is excluded from default CI.

## Diagnose a resumed history

### Recover a real interrupted tool turn

This journey creates fresh data and needs no personal session fixture:

```powershell
python e2e/thinking/run.py C:/path/to/scode.exe --model claude-opus-4-8 --preserve-thinking --thinking-display summarized --effort high --check-context --crash-after-read --artifacts C:/path/to/artifacts
```

The model actually reads an old-task file. The recording proxy pauses its next
request after the tool result is durable, without forwarding that final-answer
request upstream. The runner forcefully stops only its own CLI process, resumes
the session with a new file task, checks the exact fresh answer, exits normally,
makes the new file unavailable, and restarts again to check persisted recall.
It also verifies the old signed history, system, tools, thinking, effort and
routing identity before funding the new task's first request. The first new
turn must close the unanswered tool turn. Four real inference requests are
expected; no tool output is fabricated. Existing history and compaction flags
cannot be combined with this fresh crash fixture.

The pre-crash response supplies the signed thinking being tested: adaptive
thinking may skip a later simple Read. The native Opus 4.8 journey passed with
four HTTP 200 responses, exact answers, the original signature after both
restarts, and cache reads on all three later requests. The executable preceding
the boundary fix failed after its first real Read, before the proxy funded the
new task. A cheaper Sonnet 4.6 run also passed under the runner's explicit
adaptive profile; it does not certify Sonnet's native CLI thinking defaults.

Windows crash simulation sends one terminating signal to the runner's owned
CLI process and waits for observed exit before reopening the durable session.

### Replay a private history

`--history-fixture <session.jsonl>` copies saved message rows into the isolated
session. The original file stays unchanged and its SHA-256 is recorded and
checked. `--history-messages 8` limits the copy to the first eight messages.
No personal conversation fixture is committed to this repository.

### Unanswered turns and old-task contamination

When a saved history ends with a user prompt or a completed tool result, the
next user request must start a new turn. CC 2.1.301's actual resume request
inserts an assistant `No response requested.` sentinel in this case. Its local
`src/utils/conversationRecovery.ts` implements the same rule. The
[Messages API reference](https://platform.claude.com/docs/en/api/typescript/messages/create)
explains that consecutive user messages are combined into a single turn.

An Opus 4.8 control kept the same 23 historical messages, tools, system prompt,
adaptive thinking, effort and route. Without the sentinel, two requests added
old profile/build claims after the correct file answer. Adding only the sentinel
made both intervening requests return exactly the requested answer. Native CC
also completed the imported-history Read journey correctly. Replacing static
agent guidance or changing high to xhigh alone did not fix the old-task prose.
These controls locate a continuation defect; they do not establish the first
historical `course` token's origin. All four sentinel controls produced zero
new standalone labels.

The shared runtime appends the sentinel before a new external turn when the
previous turn is unanswered. It preserves existing history, including tool
results and signed thinking, and leaves in-flight tool continuations alone.
The portable PTY regression
`resumed_unanswered_turns_end_before_new_requests_without_rewriting_history`
resumes a tool tail in both renderers, starts and continues a new task, exits,
resumes again, and verifies the exact old history and stable wire prefix. It
also covers a plain unanswered user prompt and a completed turn that needs no
sentinel. It runs in the ordinary workspace CI suite.

The shared CLI replay formatter omits this internal marker, as CC's
`AssistantTextMessage` does. Its model/usage provenance distinguishes it from
model output; the durable record and API message remain present. The PTY
journey checks the restored task text on screen in both renderers while the
internal marker stays absent from scrollback.

The fixed executable passed this funded Read/answer/restart workflow with the
same old history on Opus 4.8, both through a controlled route and through normal
gateway routing. Both runs made three successful API requests, replayed signed
thinking, returned exact answers, and generated no standalone labels or old-task
prose. All four later requests had positive cache reads. The previous messages
remained an exact prefix after removing cache markers; system, tools, thinking,
effort and routing identity stayed unchanged. The new regression fails against
the pre-fix executable and passes all four cases against the fixed executable.

A separate 117-message history with 164 old standalone labels still produced
new `course` text when the new request was only `continue`: both controls without
the sentinel and one of two controls with it reproduced the label. These were
raw provider response captures; none of the proposed tools were executed. The
sentinel therefore fixes the demonstrated new-task contamination without
establishing that a heavily polluted history can safely continue unchanged.

The copied history redacts API keys from every proxy profile in the selected
config, including inactive profiles. Startup records the replacement count.
The recording proxy also rejects bodies containing any of those keys before
saving or forwarding them. This protects known configured keys; review private
fixtures for other credentials before replaying them through a provider.

`credential_replay.py` runs the same funded journey with a harmless key from
an inactive test profile embedded in the legacy fixture. It requires one
redaction and sends a separate local request containing that harmless key to
verify rejection before forwarding. It checks that all three client and upstream request bodies retain the
visible answer and replacement marker, and verifies that the source stayed
unchanged. The temporary credential config is removed even if the run fails.

```powershell
python e2e/thinking/credential_replay.py C:/path/to/scode.exe --profile sudorouter --upstream-channel 52 --drop-global-cache-scope --artifacts C:/path/to/artifacts
```

The runner captures startup scrollback before sending a message. With
`--require-replay-label card`, it requires the historical `card` to be visible
while the recorded API message request count is still zero. This distinguishes
old transcript replay from text generated by a new response.

Use `--expect-compaction` only for a fixture expected to compact before Read.
It requires exactly four requests: a complete text-only `<summary>`, Read,
the exact answer, and the answer after restart. The fresh workflow still requires
three requests. Both workflows check the signed block from the actual Read turn,
its exact preservation in both subsequent histories, the fresh values and nonce,
and the absence of newly generated artifact words.

```powershell
python e2e/thinking/run.py C:/path/to/scode.exe --profile sudorouter --thinking-display summarized --effort high --check-context --history-fixture C:/path/to/session.jsonl --history-messages 8 --expect-compaction --require-replay-label card --upstream-channel 52 --drop-global-cache-scope --artifacts C:/path/to/artifacts
```

The two transport flags are explicit diagnostic controls. `--upstream-channel`
uses new-api's existing admin token suffix to select a channel.
`--drop-global-cache-scope` removes only `system[0].cache_control.scope` when it
is `global`; some routes reject this marker after tools with narrower scope.
Both flags default off and their values appear in the startup record. A pass
with these controls does not validate the CLI's unmodified transport on that
route, another channel, or the first cause of historical artifact text.

`fixtures/empty-legacy-signature.jsonl` represents old records with an empty
signature string. These unsigned blocks must be omitted from outgoing history;
the original assistant text must survive. Run the same real Read/restart journey
with `--history-fixture e2e/thinking/fixtures/empty-legacy-signature.jsonl`.
The provider's newly issued signed thinking must still survive both continuations.
The portable CI regression also restores empty legacy signatures before checking
valid empty, summarized and redacted thinking through two turns and restart.

# Live signed-thinking acceptance (Windows)

This runner drives a real `scode` process through Windows ConPTY and a real
Anthropic-compatible HTTPS route. It reads fresh random values with Read,
calculates their sum, exits, removes the input file, resumes the saved session,
and subtracts 17 using the persisted result.

It checks the actual tool result and both outgoing assistant histories against
the empty signed thinking block received in the original raw SSE. Missing,
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
```

Credentials come from the named proxy profile in
`~/.nexus/sudocode/sudocode.json`; use `--config` for another file. The child
process has an isolated home/config and only a placeholder key. Real keys stay
in the recording proxy's memory and are excluded from saved requests.

The proxy sets `thinking: {type: adaptive, display: omitted}` and the selected
`--effort` (`low` by default, or `high`) to obtain the target block from the real
provider. The startup output records the selected effort. It caps each response at
2048 tokens and permits at most five message requests. The expected journey
uses three requests. This validates signed-block preservation with that
explicit profile; it does not test the CLI's default thinking configuration.

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

The provider must support that profile and return an empty signed block;
otherwise the test fails instead of counting another response shape as
coverage. Workspaces are created beneath `%PUBLIC%` and must have no inherited
agent instructions. Processes, the local recording server and workspace are
cleaned up on completion or failure. `--artifacts` retains individual request
bodies, raw responses and terminal screens in a fresh directory.

The portable CI regression for block boundaries and resume lives in
`rust/crates/rusty-sudocode-cli/tests/pty_cache_prefix.rs`:
`thinking_blocks_survive_turns_and_resume_without_merging_or_dropping`.
This runner is a funded manual acceptance test and is excluded from default CI.

# Standalone nexus-A2A E2E

End-to-end tests for the standalone-`scode` ↔ nexus A2A path: a terminal `scode`
dials a real `nexusd-cluster` as a plain gRPC client and sends/receives over the
replicated `/agents/<name>/chat-with-me` DT_STREAM (`NEXUS_A2A_*` env; off by
default). The transport lives in `runtime::mailbox` +
`nexus-vfs-client`; the send half feeds `CliToolExecutor` via the same
`handle_send_message` the co-host uses.

## Layers

| Layer | Command | LLM? |
|---|---|---|
| Unit | `cargo test -p runtime --lib mailbox` | no |
| Live client round-trip | `e2e/nexus-a2a/run.sh` | no |
| Cross-node replication and PTY duet | `e2e/nexus-a2a/run-cross-node.sh` | scripted by default |
| 2-LLM co-host duet | `SUDOROUTER_API_KEY=… SCODE_BIN=… e2e/nexus-a2a/run.sh` | yes (gated) |

The **live round-trip** (`mailbox_nexus_live`, an ignored `runtime` integration
test) is the piece unit tests can't cover: it drives `Mailbox::ensure_inbox` +
`Mailbox::send` + `Mailbox::poll` through a real gRPC server and a real
DT_STREAM. `run.sh` brings the daemon up, waits for a writable single-voter
leader, and runs it; it is deterministic and always safe to run.

The cross-node harness boots two authenticated daemons. It checks an idle tail,
a peer write that wakes it, four concurrent tails receiving all 128 ordered
messages, and a reverse acknowledgement. It then runs two real scode processes
and verifies that the receiver displays the sender's message. CI runs this
workflow against the daemon release pinned in Cargo.lock. Failures include
daemon log tails; set `NEXUS_A2A_KEEP_WORK=1` to retain the temporary data and logs.

## Prereqs

- Rust toolchain and Bash. The harness runs Rust integration tests and real PTYs
  against local daemon processes.
- `run.sh` and `run-cross-node.sh` download the daemon release pinned in
  Cargo.lock. Set `NEXUSD_BIN` to use an existing matching binary.
- `run-cohost.sh` builds the co-host from this checkout unless
  `NEXUSD_COHOST_BIN` is supplied. Building it requires `protoc`; the release
  workflow uses version 3.20.2 on macOS and Windows.

## Run

```sh
e2e/nexus-a2a/run.sh                         # deterministic, no LLM
# full 2-LLM duet (real scode -> daemon-hosted co-host that LLM-replies):
SUDOROUTER_API_KEY=sk-…funded… SCODE_BIN=$(pwd)/rust/target/debug/scode \
  e2e/nexus-a2a/run.sh
```

## Notes / gotchas

### Model routing acceptance

`run-cohost.sh` builds this checkout's daemon, boots it with mTLS, provisions
`/model` using the node credential, and spawns a managed agent. Its normal mode
uses the local scripted provider and runs in Rust CI. For real model acceptance:

```sh
NEXUS_A2A_MODEL_LIVE=1 \
NEXUS_A2A_MODEL_URL=https://api.sudorouter.ai \
NEXUS_A2A_MODEL_KEY="$SUDOROUTER_API_KEY" e2e/nexus-a2a/run-cohost.sh
```

The live journey delegates a fresh VFS quote to a child, checks the parent's
JSON artifact, sends a second mailbox message that consumes it, and verifies
the resulting amount. It also reads the native requests persisted under
`/model` to prove that the parent and child crossed the mount. Task files live
in a fresh directory under the agent's replicated content mount; `/proc` alone
provides process metadata, not task storage. Missing
credentials fail this explicit live run. The disposable daemon is stopped on
exit; `NEXUS_A2A_KEEP_WORK=1` retains its logs and data for diagnosis.

The ordinary CLI journey uses the existing PTY harness and its isolated config:

```sh
cd rust
SCODE_TEST_BACKEND=live SCODE_LIVE_MODEL=claude-sonnet-4-6 \
  SCODE_LIVE_AUTH_PROFILE=sudorouter cargo test -p rusty-sudocode-cli \
  --test pty_model_workflow_live -- --ignored --nocapture
ANTHROPIC_API_KEY="$SUDOROUTER_API_KEY" ANTHROPIC_BASE_URL=https://api.sudorouter.ai \
  cargo test -p engine-host --test cohost_model_compaction live_checkpoint \
  -- --ignored --nocapture
```

The PTY journey verifies child execution, model result summarization, persisted
compaction, and a resumed artifact after removing the source file. The second
command exercises a live checkpoint through a real kernel model mount and
then refuses the next checkpoint by revoking that route. It covers the co-host
compaction client; the managed-agent mailbox has no `/compact` command.

**Driving a live `scode` needs a PTY, not a pipe.** `printf 'prompt\n' | scode`
answers and exits — fine for one shot, and `--print` is the supported form of
it. A *persistent* receiver is the other half of a duet, and feeding it through
a pipe or a FIFO does not work: the REPL reads keys from the terminal, so the
line is never consumed, while the process stays alive looking idle. The A2A
receiver in that process still runs — its cursor advances — so the session looks
healthy and simply ignores you. Drive it through a PTY (`pty-expect`, as
`tests/pty_agent_duet.rs` does).

**A receive cursor can outlive the stream it recorded.** `a2a-cursor-<agent>`
lives in the config home and survives a rebuilt cluster or a fresh data dir. A
saved offset ahead of the new stream's tail used to park the receiver forever on
an offset that would not arrive for a long time — no error, nothing to grep.
The poller now clamps to the tail and says so on stderr, but if you want a
receiver to start clean, give it its own `SUDO_CODE_CONFIG_HOME`.


- **`--identity-dir` fresh each run.** The compose passes fresh
  `--data-dir`/`--identity-dir` so the founder boots as a clean single voter
  (quorum = 1). A stale identity (or a *coexisting* host `nexusd-cluster` on a
  port Docker Desktop forwards into the container, e.g. `serve-local --port
  12022`) shows up as `Raft message send failed … localhost:12022` noise; it is
  benign (the daemon stays writable) but for clean logs run no other host daemon
  on a forwarded port.
- **Leader election takes a few seconds.** `run.sh` gates on the round-trip
  passing (up to ~2 min) before asserting — a fresh container is not writable
  the instant it starts.
- **Co-host duet needs a current nexus-vfs.** The daemon-hosted co-host agent
  bridges sync→async raft calls; on a *current-thread* runtime the old
  `bridge_block_on` (`raft/src/runtime_bridge.rs`) calls `block_in_place` and
  panics (`can call blocking only when running on the multi-threaded runtime`).
  Fixed on nexus-vfs main (flavor-aware `lib::rt`); the co-host image must pin a
  nexus-vfs at/after that fix. `scode`'s send half is unaffected — it reads/writes
  the DT_STREAM directly.

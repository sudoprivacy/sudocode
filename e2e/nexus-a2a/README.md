# Standalone nexus-A2A E2E

End-to-end tests for the standalone-`scode` ↔ nexus A2A path: a terminal `scode`
dials a real `nexusd-cluster` as a plain gRPC client and sends/receives over the
replicated `/agents/<name>/chat-with-me` DT_STREAM (`NEXUS_A2A_*` env; off by
default). The transport lives in `runtime::nexus_mailbox` +
`nexus-vfs-client`; the send half feeds `CliToolExecutor` via the same
`handle_send_message` the co-host uses.

## Layers

| Layer | Command | LLM? |
|---|---|---|
| Unit | `cargo test -p runtime --lib nexus_mailbox` | no |
| Live client round-trip | `e2e/nexus-a2a/run.sh` | no |
| 2-LLM co-host duet | `SUDOROUTER_API_KEY=… SCODE_BIN=… e2e/nexus-a2a/run.sh` | yes (gated) |

The **live round-trip** (`nexus_mailbox_live`, an ignored `runtime` integration
test) is the piece unit tests can't cover: it drives `ensure_stream` +
`stream_write` + `stream_read_at` through a real gRPC server and a real
DT_STREAM. `run.sh` brings the daemon up, waits for a writable single-voter
leader, and runs it; it is deterministic and always safe to run.

## Prereqs

- Docker (a nexus daemon image — a nexus artifact, not built here). Default
  `nexusd-cluster-cohost:latest`; override with `NEXUS_DAEMON_IMAGE`. Build once
  from the nexus repo (see `dockerfiles/Dockerfile.nexusd-{cluster,cohost}`).
- Rust toolchain (the harness runs the `runtime` integration tests on the host
  against the containerized daemon).

## Run

```sh
e2e/nexus-a2a/run.sh                         # deterministic, no LLM
# full 2-LLM duet (real scode -> daemon-hosted co-host that LLM-replies):
SUDOROUTER_API_KEY=sk-…funded… SCODE_BIN=$(pwd)/rust/target/debug/scode \
  e2e/nexus-a2a/run.sh
```

## Notes / gotchas

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

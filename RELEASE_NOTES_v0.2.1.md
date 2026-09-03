# Sudo Code v0.2.1

## What's New

### Features
- **Unified agent/pid tool system** (#536, #542) — 7 new tools aligned to the nexus agent/pid model: `agent_list`, `agent_spawn`, `send`, `pid_fork`, `pid_output`, `pid_status`, `pid_kill`. These replace the ad-hoc `Agent`, `SendMessage`/`send_message`, `TaskStop`, `TaskGet`, `TaskList`, `TaskOutput` tools with a consistent `{layer}_{verb}` naming convention. All old names are retained as deprecated aliases — zero breaking changes.
- **Per-project account switching** (#541) — `auth_profile` in layered config lets each project use a different API key / auth mode without env-var juggling. Config is now layered: global → project → env, with the first explicit value winning.
- **A2A blocking tail** (#526, #525) — both co-hosted and standalone scode receivers now wait on the kernel's blocking `DT_STREAM` tail instead of polling, cutting idle CPU and improving message delivery latency.

### Changes
- **Co-host state observer arity** (#531) — `state_callback` now takes `(AgentState, Option<String>)` so agents can report a reason string alongside state transitions (e.g. "awaiting input").
- **nexus-vfs pin bumps** (#524, #527, #528) — three successive pins bringing in `NEXUS_ALLOW_HOSTNAME_ADVERTISE` opt-out, `sys_write` wakes `DT_STREAM` tail, and `ServiceBootCtx` widen.

### Fixes
- **PTY test deflake** — `config_tree` exit test settles overlay teardown before `/exit` to avoid race conditions.

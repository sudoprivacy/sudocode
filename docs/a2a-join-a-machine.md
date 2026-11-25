# Join a machine to the agent network

What a machine needs, in order, before a Codex or Claude Code session on it can
exchange messages with other agents. Each step says who does it.

| Step | What | Done by |
| --- | --- | --- |
| 1 | A Nexus node running on the machine and joined to the cluster | The Nexus installation. Not covered here. |
| 2 | `scode`, configured with an account | The scode installation |
| 3 | Node.js 20 or newer | The machine's owner |
| 4 | The frontend attached | One scode command per frontend, below |

scode sessions need only steps 1 and 2. Steps 3 and 4 are for Codex and Claude
Code, whose sessions join through the bridge, a Node.js program; each attached
agent still runs an `scode` process underneath.

A release build of scode carries the bridge package (`@sudocode/acp`) and
writes it to `~/.local/share/sudocode/acp/versions/<commit>/` the first time
the command runs, so nothing else has to be transferred to the machine and the
two versions always match. A build made without it asks for `--sdk` with the
folder of a package installed by hand (see `sdk/typescript/README.md`).

## Claude Code

```sh
scode a2a setup claude --issuer https://HOST:2126
```

- The command starts no session and calls no model, so a person or any agent
  can run it from a shell.
- `--issuer` names the node that issues agent names (the cluster founder). The
  node on this machine was started with a `--peers` argument naming the same
  host and port. It is needed once; later runs need no flags.
- The command prints one JSON object. Read `instructions` and follow them in
  order; `next` holds exact commands when there is something to run.
- `state` is one of `installed`, `already_installed`, `moved_to_this_sdk`,
  `needs_input` (a flag is missing; nothing was written), `refused` (something
  else is already attached; the instructions say how to remove it) or `failed`.
- Then start a new Claude Code session and call `scode_agent_list` in it. A list
  of agents means the session has joined; other agents see it as
  `Claude Code · <machine> · <folder>` under a name starting with `claude-`.

## After an SDK update

Run the same command with the new scode. It writes the new package beside the
old one, moves the registration and hooks to it, and keeps every session's
identity. Sessions that are already open keep the old package until they are
restarted, so the old version folder is left in place.

## Undo

1. `claude mcp remove nexus_scode -s user`
2. In Claude Code's `settings.json`, delete the hook entries whose command
   contains `--claude-watch` or `--claude-deliver`. The copy from before setup
   is under `<config root>/claude/backups/`.
3. The config root (default `~/.config/sudocode/acp/a2a`) holds the enrollment
   grant, the operator record and each session's identity. Removing it makes
   every session enroll a new agent name on its next start; the old names stay
   on the network.

---
name: Feature request
about: Propose a new capability, command, or workflow for Sudo Code (`scode`)
title: "[feature] <short summary>"
labels: ["enhancement", "needs-triage"]
assignees: []
---

<!--
Thanks for proposing an idea. Strong feature requests describe the user-visible
behavior and the motivating use case BEFORE jumping to implementation details —
that gives maintainers and the community room to suggest alternatives.

Before submitting:
  - Search existing issues (open AND closed) — including discussions — for
    similar proposals.
  - Check README.md, USAGE.md, and docs/ to confirm the feature doesn't already
    exist under a different name.
-->

## Summary (required)

<!-- One or two sentences describing the proposed feature. -->

## Problem / motivation (required)

<!--
What user problem does this solve? Who hits it? Concrete examples help a lot.
Avoid framing as "we should add X" — frame as "users currently can't do Y".
-->

## Proposed solution

<!--
The user-visible behavior you have in mind. CLI flags, commands, config keys,
API endpoints, tool names, etc. Mock command lines or sample output are great.
-->

```bash
# Example of how this would be invoked / used
scode ...
```

## Affected area

<!-- Tick all that apply. -->

- [ ] `scode` CLI / REPL
- [ ] Agent runtime (`crates/runtime`)
- [ ] Built-in tools (`crates/tools`)
- [ ] Plugins (`crates/plugins`)
- [ ] API / ACP surface (`crates/api`)
- [ ] Commands / slash commands (`crates/commands`)
- [ ] RAG / indexing (`crates/rag`)
- [ ] Telemetry (`crates/telemetry`)
- [ ] Documentation
- [ ] New crate / subsystem
- [ ] Other / unsure

## Alternatives considered

<!--
Other ways to solve the same problem. Workarounds you've tried. Reasons why
this proposal is preferable. Saying "I considered X and it doesn't work because
Y" is genuinely valuable signal.
-->

## Compatibility / migration

<!--
Would this change existing behavior, CLI flags, config files, on-disk formats,
or the ACP surface? If yes, what's the migration story for existing users?
-->

- [ ] Pure addition (no breaking change)
- [ ] Behavior change behind a flag / opt-in
- [ ] Breaking change (please justify)

## Are you willing to contribute?

- [ ] I'd like to implement this myself and would appreciate guidance.
- [ ] I can help with design / review but won't write the code.
- [ ] I'm just filing the request.

## Additional context

<!-- Links to prior art in other tools, related issues, screenshots, etc. -->

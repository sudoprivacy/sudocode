---
name: Bug report
about: Report a defect, crash, or incorrect behavior in Sudo Code (`scode`)
title: "[bug] <short summary>"
labels: ["bug", "needs-triage"]
assignees: []
---

<!--
Thanks for taking the time to file a bug. Please fill in as much of the form
as you can — fields marked (required) are needed before we can triage.

Before submitting:
  - Search existing issues (open AND closed) for duplicates.
  - Try the latest `main` build if you're on a released version.
  - If this is a security issue, do NOT file a public report — see SECURITY.md
    or use GitHub's private vulnerability reporting.
-->

## Summary (required)

<!-- One or two sentences: what is broken and what did you expect instead? -->

## Affected area

<!-- Tick all that apply. Helps us route the issue. -->

- [ ] `scode` CLI / REPL
- [ ] Agent runtime (`crates/runtime`)
- [ ] Built-in tools (`crates/tools`)
- [ ] Plugins (`crates/plugins`)
- [ ] API / ACP surface (`crates/api`)
- [ ] Mock service / parity harness (`crates/mock-anthropic-service`)
- [ ] RAG / indexing (`crates/rag`)
- [ ] Telemetry (`crates/telemetry`)
- [ ] Build / CI / tooling
- [ ] Documentation
- [ ] Other / unsure

## Steps to reproduce (required)

<!--
Minimal, copy-pasteable reproduction. If credentials are needed, redact them
but say which auth mode you used (api-key / subscription / proxy).
-->

1. `cd rust/`
2. `cargo build --release`
3. `./target/release/scode ...`
4. ...

## Expected behavior (required)

<!-- What you thought would happen. -->

## Actual behavior (required)

<!-- What actually happened. Paste full error output / stack trace in the block below. -->

<details>
<summary>Logs / output</summary>

```text
<paste here>
```

</details>

## Environment (required)

Please run the following from the repo root and paste the output:

```bash
cd rust/
./target/release/scode --version 2>/dev/null || cargo run --bin scode -- --version
rustc --version
cargo --version
uname -a    # or `ver` on Windows
```

| Field | Value |
|---|---|
| `scode` version / commit | |
| `rustc` version | |
| `cargo` version | |
| OS & arch | |
| Auth mode (`api-key` / `subscription` / `proxy`) | |
| Model alias / id (`opus` / `sonnet` / `haiku` / `grok` / ...) | |
| Installed via (source build / release binary / package) | |

## Regression?

<!-- If this used to work, when did it break? Commit hash, version, or "unsure". -->

- Last known good version/commit:
- First broken version/commit:

## Reproduction steps you've tried

<!-- Optional. Things that didn't help, workarounds you found, etc. -->

## Additional context

<!-- Screenshots, config snippets, related issues, anything else relevant. -->

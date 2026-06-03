---
name: browser
description: "AI-native browser. Explore websites, discover page structure, take screenshots, and automate interactions via the `browser` CLI."
---

# Browser

`browser <noun> <verb> [flags]` wraps ai-dev-browser's tools. Data goes to
stdout, errors to stderr. Requires a Chrome/Chromium install for live browsing.

## Output modes (every command)

- `--json`   machine-readable JSON.
- `--quiet`  just the primary scalar (port, url, path, count, ...).
- `--full`   dump every field (default output is terse: ~3-4 fields/item).
- `--no-interactive`  never prompt; missing required values error out.

When stdout is not a TTY, `--json --quiet --no-interactive` are auto-enabled,
so output is always pipe-safe and never hangs.

## Discover commands

```bash
browser --help                 # list nouns
browser page --help            # list verbs under `page`
browser page goto --help       # flags for one command
```

## 5 most-used commands

```bash
# 1. Start (or reuse) a browser — idempotent: reuses an idle Chrome by default
browser session start --json
browser session start --headless --json

# 2. Navigate
browser page goto --url https://example.com --json

# 3. See what's interactable (returns refs for click/type by ref)
browser page discover --json

# 4. Click / type
browser click text --text "Sign in" --json
browser type text --text "hello" --selector "#search" --json

# 5. List sessions / tabs
browser session list --json
browser tab list --json
```

## Exit codes

`0` ok · `2` validation · `4` not-found (element or no browser) · `5` conflict
· `7` auth · `8` rate-limit · `9` transient (timeout, connection). Never `1`
for everything — branch on the code.

In `--json` mode an error is printed to **stderr** as:

```json
{"error": {"code": 4, "message": "...", "retryable": true, "hint": "..."}}
```

## Common error recoveries

- `code 4` + "Failed to connect to Chrome" → no browser running. Run
  `browser session start` first (retryable).
- `code 4` + "not found" → the element isn't there. Re-run
  `browser page discover` and use a fresh `--ref`, or widen your selector.
- `code 9` + "timeout" → bump `--timeout`, or `browser page wait-ready`
  before acting; then retry.

## Notes

- Noun-verb tree: session, page, tab, click, find, type, element, mouse,
  cookies, storage, window, js, cdp, dialog, download, login.
- `browser login interactive` is human-in-the-loop; it fails fast (code 7)
  under `--no-interactive` or when stdout is not a TTY.
- Every CLI command has an identical Python function in `ai_dev_browser.core`
  for scripting, e.g. `from ai_dev_browser.core import page_goto, click_by_text`.

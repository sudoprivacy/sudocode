---
name: browser
description: "AI-native browser. Explore websites, discover page structure, take screenshots, and automate interactions via the `browser` CLI."
---

# Browser

`browser <noun> <verb> [flags]` — control a headless or headed Chrome session.

## Discover commands

```bash
browser --help               # list all nouns
browser <noun> --help         # list verbs under a noun
```

## Quick-start examples

```bash
browser session start --json                          # launch (or reuse) a Chrome session
browser page goto --url https://example.com --json    # navigate
browser page discover --json                          # list interactable elements
browser click text --text "Sign in" --json            # click by visible text
browser tab list --json                               # list open tabs
```

When stdout is not a TTY, `--json` mode is auto-enabled so output is always pipe-safe.

Every CLI command has an identical Python function in `ai_dev_browser.core` — explore interactively with CLI, then script with the same functions:

```python
from ai_dev_browser.core import page_goto, click_by_text, page_discover
```

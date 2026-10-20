---
name: browser
description: "Explore websites, inspect page structure, take screenshots, and automate interactions using ai-dev-browser."
---

# Browser

Use the installed `ai-dev-browser` package. Its CLI modules are generated from
`ai_dev_browser.core`; use the same functions for Python automation.
Install the package in your Python environment before invoking the tools.

## Discover commands

```bash
python3 -m ai_dev_browser.tools.browser_start --help
python3 -m ai_dev_browser.tools.page_discover --help
```

## Quick-start examples

```bash
python3 -m ai_dev_browser.tools.browser_start
python3 -m ai_dev_browser.tools.page_goto --session-id <returned-id> --url https://example.com
python3 -m ai_dev_browser.tools.page_discover --session-id <returned-id>
python3 -m ai_dev_browser.tools.click_by_text --session-id <returned-id> --text "Sign in"
python3 -m ai_dev_browser.tools.page_screenshot --session-id <returned-id> --path screenshot.png
python3 -m ai_dev_browser.tools.browser_stop --session-id <returned-id>
```

Choose headless/headful from the task and user preference; use `browser_connect`
to attach to an existing browser. Connecting preserves its current mode.
Use the session identity returned by start/connect for subsequent commands.
`browser_stop` closes an owned browser; use `browser_disconnect` to release
control of an attached browser while preserving its windows and tabs. Consult
`--help` for the selected tool before configuring transport or profile settings.
To inspect screenshot pixels, use scode's `Read` tool on the saved image.

The same operations are available from Python:

```python
from ai_dev_browser.core import page_goto, click_by_text, page_discover
```

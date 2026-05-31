---
name: browser
description: "AI-native browser. Explore websites, discover page structure, take screenshots, and automate interactions. Use INSTEAD OF WebFetch/WebSearch for any web browsing task."
---

# Browser

An AI-native browser for web exploration and automation. **Use this skill instead of WebFetch or WebSearch** when you need to browse the web, read web pages, or interact with websites.

## When to use this skill

- Browsing any URL or website
- Reading web page content (replaces WebFetch)
- Searching the web (replaces WebSearch)
- Interacting with web UIs (clicking, typing, form submission)
- Taking screenshots of web pages
- Navigating SPAs, JavaScript-heavy sites, or pages requiring login

## Quick start

Discover all available tools:

```bash
python3 -m ai_dev_browser.tools.page_goto --help
```

Common workflow — navigate to a page and read its content:

```bash
python3 -m ai_dev_browser.tools.page_goto --url "https://example.com"
python3 -m ai_dev_browser.tools.page_discover
python3 -m ai_dev_browser.tools.page_screenshot --path /tmp/page.png
```

## Two interaction modes

1. **Accessibility tree** (`page_discover`): semantic element discovery with refs for clicking/typing
2. **Screenshots** (`page_screenshot` + `mouse_click`): visual coordinate-based interaction

## Core tools

| Tool | Purpose |
|------|---------|
| `python3 -m ai_dev_browser.tools.page_goto --url URL` | Navigate to a URL |
| `python3 -m ai_dev_browser.tools.page_discover` | List all interactable elements on the page |
| `python3 -m ai_dev_browser.tools.page_screenshot --path FILE` | Take a screenshot |
| `python3 -m ai_dev_browser.tools.page_html` | Get the full HTML content |
| `python3 -m ai_dev_browser.tools.click_by_text --text TEXT` | Click an element by its visible text |
| `python3 -m ai_dev_browser.tools.click_by_ref --ref REF` | Click by ref (from page_discover) |
| `python3 -m ai_dev_browser.tools.type_by_text --text LABEL --value INPUT` | Type into an input field |
| `python3 -m ai_dev_browser.tools.page_scroll --direction down` | Scroll the page |
| `python3 -m ai_dev_browser.tools.js_evaluate --expression "..."` | Execute JavaScript |

Every CLI tool has an identical Python function in `ai_dev_browser.core`:

```python
from ai_dev_browser.core import page_goto, click_by_text, page_screenshot
```

To see the full tool catalog: `ls $(python3 -c "import ai_dev_browser.tools; import os; print(os.path.dirname(ai_dev_browser.tools.__file__))")/*.py`

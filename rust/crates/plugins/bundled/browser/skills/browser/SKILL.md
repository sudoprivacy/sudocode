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
browser --list
```

Common workflow — navigate to a page and read its content:

```bash
browser page_goto --url "https://example.com"
browser page_discover
browser page_screenshot --path /tmp/page.png
```

## Two interaction modes

1. **Accessibility tree** (`page_discover`): semantic element discovery with refs for clicking/typing
2. **Screenshots** (`page_screenshot` + `mouse_click`): visual coordinate-based interaction

## Core tools

| Tool | Purpose |
|------|---------|
| `browser page_goto --url URL` | Navigate to a URL |
| `browser page_discover` | List all interactable elements on the page |
| `browser page_screenshot --path FILE` | Take a screenshot |
| `browser page_html` | Get the full HTML content |
| `browser click_by_text --text TEXT` | Click an element by its visible text |
| `browser click_by_ref --ref REF` | Click an element by its ref (from page_discover) |
| `browser type_by_text --text LABEL --value INPUT` | Type into an input field |
| `browser page_scroll --direction down` | Scroll the page |
| `browser js_evaluate --expression "..."` | Execute JavaScript |

Every CLI tool has an identical Python function in `ai_dev_browser.core`:

```python
from ai_dev_browser.core import page_goto, click_by_text, page_screenshot
```

Run `browser --list` for the full tool catalog with descriptions.

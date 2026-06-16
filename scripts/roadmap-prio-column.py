#!/usr/bin/env python3
# One-off: add "Hacker Priority" column to every inventory table in
# ROADMAP.html, refine 3 row descriptions (single-turn / streaming /
# graceful cancel), and drop the agent-switching row. Lives under
# scripts/ purely so the diff is reproducible; not part of the build.

import re, sys, io, pathlib

PATH = pathlib.Path("ROADMAP.html")
src = PATH.read_text(encoding="utf-8")

# Refine §1 Core conversation rows ---------------------------------------------
src = src.replace(
    '<tr><td>Single-turn prompt</td><td>Gap</td><td>—</td><td></td></tr>',
    '<tr><td>Single-turn prompt</td><td>Gap</td><td>—</td><td>'
    'Non-interactive, invocation-driven: <code>scode -p "…"</code> starts, '
    'streams the assistant turn, then exits. PTY verifies the process truly '
    'exits without TTY input.</td></tr>'
)
src = src.replace(
    '<tr><td>Streaming response</td><td>Gap</td><td>—</td><td>Implicit in every interaction</td></tr>',
    '<tr><td>Streaming response</td><td>Gap</td><td>—</td><td>'
    'LLM-API streaming: assistant tokens render incrementally as they arrive. '
    'Pairing requirement for cancel-mid below.</td></tr>'
)
src = src.replace(
    '<tr><td>Graceful cancel mid-execution</td><td>Gap</td><td>—</td><td>'
    'Integration-covered by <code>interrupt_e2e.rs</code>; PTY pending — '
    'SIGINT during bash, verify interrupted envelope and no continuation</td></tr>',
    '<tr><td>Graceful cancel mid-execution</td><td>Gap</td><td>—</td><td>'
    'User presses ESC during streaming output (LLM tokens or tool call) — '
    'scode stops the in-flight turn and no further iterations issue. '
    'Integration-covered by <code>interrupt_e2e.rs</code>; PTY pending — '
    'SIGINT during bash, verify interrupted envelope and no continuation.</td></tr>'
)

# Drop the agent-switching row (sudowork-UI concern, per shareone comment).
src = re.sub(
    r'\s*<tr><td>Agent switching \(Sudoclaw ↔ scode\)</td>'
    r'<td>N/A</td><td>—</td><td>sudowork-UI concern</td></tr>\n',
    '\n',
    src
)

# Add "Hacker Priority" column to every inventory table -------------------------
# Inventory tables sit between <h3>Feature inventory</h3> and the next
# <h3>Coverage denominator</h3>. Only those tables get the new column.
start = src.index('<h3>Feature inventory</h3>')
end = src.index('<h3>Coverage denominator')
head_block = src[start:end]

# Rewrite thead — Feature | Hacker Priority | Status | Test | Notes
head_block = head_block.replace(
    '<thead><tr><th>Feature</th><th>Status</th><th>Test</th><th>Notes</th></tr></thead>',
    '<thead><tr><th>Feature</th><th>Hacker Priority</th><th>Status</th>'
    '<th>Test</th><th>Notes</th></tr></thead>'
)

# Tag each tbody row with a priority td derived from its status / L2 marker.
#   - Status == "N/A"   → "N/A"     (out of scope for scode)
#   - "L2" in status    → "nice"    (L2-deferred = not blocking for hacker MVP)
#   - everything else   → "must-have"
def add_prio(match):
    feature_td, status_td, rest = match.group(1), match.group(2), match.group(3)
    status_text = re.sub(r'<[^>]+>', '', status_td)
    if 'N/A' in status_text:
        prio = 'N/A'
    elif 'L2' in status_text:
        prio = 'nice'
    else:
        prio = 'must-have'
    return f'<tr><td>{feature_td}</td><td>{prio}</td><td>{status_td}</td>{rest}'

row_re = re.compile(
    r'<tr><td>(.*?)</td><td>(Gap[^<]*|Covered[^<]*|N/A[^<]*)</td>(.*?</tr>)',
    re.DOTALL
)
head_block = row_re.sub(add_prio, head_block)

src = src[:start] + head_block + src[end:]

# Insert the legend right after the "Status values:" bullet list, so the new
# column is documented inline.
status_marker = ("  <li><strong>N/A</strong> — out of scope for "
                 "<code>scode</code> itself.</li>\n</ul>")
legend = (
    "  <li><strong>N/A</strong> — out of scope for <code>scode</code> itself.</li>\n"
    "</ul>\n\n"
    "<p><strong>Hacker priority</strong> (the new column):</p>\n"
    "<ul>\n"
    "  <li><strong>must-have</strong> — the hacker workflow blocks without "
    "it: invocation, streaming feedback, mid-flight cancel, bash/edit/grep, "
    "<code>/commit</code>, <code>/pr</code>, <code>/resume</code>, "
    "<code>--output-format json</code>.</li>\n"
    "  <li><strong>nice</strong> — L2-deferred surface (LSP, sub-agents, "
    "cron, MCP plugins): real features, not on the critical path for "
    "e2e &ge; 90%.</li>\n"
    "  <li><strong>N/A</strong> — sudowork-UI / credential-injection "
    "concerns; never enters <code>scode</code>'s coverage denominator.</li>\n"
    "</ul>\n\n"
    "<p>The aggregated tier view (P0/P1/P2/P3 across categories) still "
    "lives in <a href=\"#priority-sequencing\">Priority sequencing</a>; "
    "the per-row column above is the hacker-priority lens "
    "&mdash; one decision per feature, not one decision per tier.</p>"
)
assert status_marker in src, "status_marker not found"
src = src.replace(status_marker, legend, 1)

# Anchor the Priority sequencing h3 so the legend link resolves.
src = src.replace(
    '<h3>Priority sequencing</h3>',
    '<h3 id="priority-sequencing">Priority sequencing</h3>',
    1,
)

PATH.write_text(src, encoding="utf-8")
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
print("OK")

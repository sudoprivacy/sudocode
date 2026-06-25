#!/usr/bin/env python3
"""Triage claw-code commits since LAST_PARITY_SYNC_COMMIT.

Categorizes each non-merge commit by pattern into the resolution buckets
defined in ROADMAP.html (Goal 2 - Resolution taxonomy). Writes a
UTF-8 Markdown report grouped by category.

Run from sudocode repo root after cloning claw-code locally:

    python3 scripts/triage_claw_code.py \\
        --claw ../claw-code \\
        --out docs/parity-claw-code-sync-2026-W24.md
"""

import argparse
import io
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--claw",
        type=Path,
        default=Path("../claw-code"),
        help="path to local claw-code clone (default: ../claw-code)",
    )
    parser.add_argument(
        "--out",
        type=Path,
        help="output Markdown path (default: stdout, UTF-8)",
    )
    args = parser.parse_args()

    claw_path = args.claw.resolve()
    sync_sha = Path("LAST_PARITY_SYNC_COMMIT").read_text().strip()

    log = subprocess.check_output(
        [
            "git",
            "-C",
            str(claw_path),
            "log",
            f"{sync_sha}..HEAD",
            "--no-merges",
            "--pretty=format:%h|%ai|%s",
        ],
        text=True,
        encoding="utf-8",
    )

    commits = []
    for line in log.splitlines():
        if not line.strip():
            continue
        sha, date, subject = line.split("|", 2)
        commits.append((sha, date[:10], subject))

    # Categorization rules (order matters; first match wins).
    rules = [
        # (tag, label, predicate)
        (
            "SKIP-omx",
            "claw-code internal multi-agent orchestration",
            lambda s: re.search(r"^omx\(team\)|auto-checkpoint|worker-\d", s)
            or re.search(r"merge worker-\d|stabilize.*worker|G\d{3}.*worker", s, re.I),
        ),
        (
            "SKIP-g004",
            "claw-code internal architecture (G004 / approval token / lane events)",
            lambda s: re.search(
                r"\bG00\d|approval[_ -]?token|lane[_ -]?event|report[_ -]?contract|contract.*bundle|conformance.*harness",
                s,
                re.I,
            ),
        ),
        (
            "SKIP-provider",
            "provider-specific (we use sudorouter)",
            lambda s: re.search(
                r"\bollama|\bqwen|\bdeepseek|\bkimi|\bglm|gemini|openai[_ -]?compat|dashscope|grok|providers?\b.*(fix|feat|test)",
                s,
                re.I,
            ),
        ),
        (
            "PICK-hint",
            "typed error envelope / hint field — candidate (verify CC alignment)",
            lambda s: re.search(
                r"hint[_ -]?field|error[_ -]?kind|typed[_ -]?error|envelope|non[_ -]?null hint|interactive[_ -]?only",
                s,
                re.I,
            ),
        ),
        (
            "SKIP-claw-internal",
            "claw-code internal probes / dogfood / branding",
            lambda s: re.search(
                r"dogfood|ultragoal|gajae|gaebal|stale.*probe|leftover.*ultra|museum exhibit|claw[- ]code|brand",
                s,
                re.I,
            ),
        ),
        # Likely-claude-code-aligned topical buckets
        (
            "PICK-tool",
            "tool surface (bash / file ops / web / agent / etc.)",
            lambda s: re.search(
                r"\b(bash|read[_ -]?file|write[_ -]?file|edit[_ -]?file|grep[_ -]?search|glob[_ -]?search|web[_ -]?(search|fetch)|task[_ -]?create|task[_ -]?get|notebook[_ -]?edit|todo[_ -]?write|skill)\b",
                s,
                re.I,
            )
            and not re.search(r"claw|ultraworker", s, re.I),
        ),
        (
            "PICK-slash",
            "slash command surface",
            lambda s: re.search(
                r"/[a-z]+|slash[_ -]?command|repl[_ -]?command|\bcommands?[_ -]?(add|new|fix)",
                s,
                re.I,
            ),
        ),
        (
            "PICK-acp",
            "ACP transport / SDK surface",
            lambda s: re.search(r"\bacp\b|jsonrpc|websocket|sse[_ -]?stream", s, re.I),
        ),
        (
            "PICK-permissions",
            "permission / sandbox / safety",
            lambda s: re.search(
                r"permission|sandbox|danger[_ -]?full|workspace[_ -]?write|read[_ -]?only|trust",
                s,
                re.I,
            ),
        ),
        (
            "PICK-session",
            "session / resume / compact / config",
            lambda s: re.search(
                r"\bsession|\bresume|\bcompact|\bcompaction|\bconfig|\bsettings|memory[_ -]?file|claude\.md",
                s,
                re.I,
            ),
        ),
        (
            "PICK-mcp",
            "MCP server / plugin / hook lifecycle",
            lambda s: re.search(
                r"\bmcp\b|plugin|\bhook|stdio[_ -]?server", s, re.I
            ),
        ),
        (
            "PICK-tui",
            "TUI / rendering / streaming display",
            lambda s: re.search(
                r"\btui|render|stream(ing)?|spinner|markdown|crossterm|ratatui|color|theme",
                s,
                re.I,
            ),
        ),
        (
            "PICK-cli",
            "CLI flags / arg parsing / output format",
            lambda s: re.search(
                r"\bcli\b|\bflag|--[a-z-]+|arg[_ -]?pars|output[_ -]?format|json[_ -]?output",
                s,
                re.I,
            ),
        ),
        (
            "PICK-doctor",
            "diagnostics / doctor / status / cost",
            lambda s: re.search(r"\bdoctor|/status|/cost|/sandbox|telemetry", s, re.I),
        ),
        # Catch-all
    ]

    buckets = defaultdict(list)
    for sha, date, subj in commits:
        matched = False
        for tag, label, predicate in rules:
            if predicate(subj):
                buckets[(tag, label)].append((sha, date, subj))
                matched = True
                break
        if not matched:
            buckets[("REVIEW", "needs manual triage")].append((sha, date, subj))

    lines = []

    def emit(s=""):
        lines.append(s)

    emit("# claw-code cherry-pick triage")
    emit()
    emit(
        f"Source: `ultraworkers/claw-code` since `{sync_sha[:12]}` "
        "(LAST_PARITY_SYNC_COMMIT)."
    )
    emit()
    emit(f"Total non-merge commits in window: **{len(commits)}**.")
    emit()
    emit("## Summary by category")
    emit()
    emit("| Tag | Description | Count |")
    emit("|---|---|---|")

    order = [
        "SKIP-omx",
        "SKIP-g004",
        "SKIP-provider",
        "SKIP-claw-internal",
        "PICK-hint",
        "PICK-tool",
        "PICK-slash",
        "PICK-acp",
        "PICK-permissions",
        "PICK-session",
        "PICK-mcp",
        "PICK-tui",
        "PICK-cli",
        "PICK-doctor",
        "REVIEW",
    ]
    by_tag = {tag: [] for tag in order}
    for (tag, label), items in buckets.items():
        by_tag.setdefault(tag, []).extend(items)

    for tag in order:
        if tag not in by_tag:
            continue
        items = by_tag[tag]
        # find label
        label = next(
            (lbl for (t, lbl) in buckets if t == tag),
            "(no items)",
        )
        emit(f"| `[{tag}]` | {label} | {len(items)} |")

    for tag in order:
        if tag not in by_tag or not by_tag[tag]:
            continue
        items = by_tag[tag]
        label = next((lbl for (t, lbl) in buckets if t == tag), "")
        emit()
        emit(f"## `[{tag}]` - {label} ({len(items)})")
        emit()
        emit("| Date | SHA | Subject |")
        emit("|---|---|---|")
        for sha, date, subj in items:
            esc = subj.replace("|", "\\|")
            emit(f"| {date} | `{sha}` | {esc} |")

    body = "\n".join(lines) + "\n"

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(body, encoding="utf-8")
        print(f"wrote {args.out} ({len(commits)} commits, {len(by_tag)} buckets)")
    else:
        # Force UTF-8 stdout on Windows consoles.
        sys.stdout = io.TextIOWrapper(
            sys.stdout.buffer, encoding="utf-8", line_buffering=True
        )
        sys.stdout.write(body)


if __name__ == "__main__":
    main()

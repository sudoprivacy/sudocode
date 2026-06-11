# Plans

Design plans for `scode`, split by lifecycle.

## Active

In-flight plans for work currently being scoped, designed, or built.

- [`active/tui-enhancement.md`](./active/tui-enhancement.md) — a phased
  approach to the terminal UI for `rusty-sudocode-cli`.

## Archive

Plans whose scope has landed or been superseded. Kept for historical
context.

- [`archive/2026-05-20-log-optimization.md`](./archive/2026-05-20-log-optimization.md) —
  log system overhaul.
- [`archive/2026-06-08-spike-179.md`](./archive/2026-06-08-spike-179.md) —
  investigation for the three spike threads attached to issue #179
  (diff-aware `edit_file` display, `/search` scope, `/undo` data
  round-trip). Its answers feed Phases 3.4, 4.3, and 4.4 of the TUI
  plan.

## Conventions

- Active plans live as a flat list under `active/`. Filenames are
  lower-kebab-case slugs.
- Archived plans use a `YYYY-MM-DD-` date prefix to make the timeline
  scannable.
- When a plan ships or is superseded, move the file into `archive/`
  and update its index entry above.

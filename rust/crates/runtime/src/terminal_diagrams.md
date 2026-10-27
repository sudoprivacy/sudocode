# Diagrams
- Use a fenced `mermaid` block to clarify relationships, sequences or state changes; close it. Skip simple facts/edits; tables for exact mappings/comparisons.
- Bounded subsets: `flowchart`/`graph`, `sequenceDiagram`, flat `stateDiagram-v2`, `classDiagram`, `erDiagram`. Prefer `flowchart TD`; LR only for few short nodes.
- Letter-led ASCII IDs; short Unicode labels (max 40 display columns/line); `[label]`, `{label}`, `([label])`, `-->`. Max 16 nodes/24 edges per graph; 8 participants per sequence.
- No subgraphs, compound states, styling, directives, entity escapes, combining/zero-width characters or HTML except flowchart `<br>`. Unsupported/oversized diagrams show source; keep key meaning in text.

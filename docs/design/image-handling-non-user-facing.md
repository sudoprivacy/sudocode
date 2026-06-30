# Image handling should be NON-user-facing (design)

> Status: Design draft for review · 2026-06-29 · Owner: sudowork-win-pc-0
> Tracking roadmap row: see [sudo-code-roadmap.html §9 "Image too large / wrong-model should be NON-user-facing"](../../sudo-code-roadmap.html#section-9)

## Goal

The user pastes an image in any sudowork conversation. The image **always** ends up handled — either as a real vision input or as a transparently-spliced text description. The user never sees:

- "图片太大了，请压缩后再发送" — size errors
- "当前模型不支持图片分析" — wrong-model tips
- A stuck processing spinner
- A `[Image #N was too large to send]` placeholder in chat

Any of those today is a leak from "infrastructure constraint" up to "user task interrupted". This design closes them.

## Today's reality

### The multi-backend constraint (this drives the architecture)

Sudowork is a **client** to multiple ACP backends. Different code paths per backend:

| Backend | Who owns | Vision-check tip today | Notes |
|---|---|---|---|
| `scode` (sudocode) | **us** | Yes (sudowork-side at `AcpAgent.ts:986`) | Can fully control via ACP capability + Rust runtime |
| `claude` (Claude Code) | Anthropic | No (Claude Code handles internally) | Standard ACP; we cannot extend their protocol |
| `codex` (OpenAI Codex) | OpenAI | No | Standard ACP |
| `qwen` (Qwen CLI) | Alibaba | No | Standard ACP |
| `remote-agent` (Moss) | sudowork | Depends on remote impl | Out of scope here |

**Implication**: the SSOT cannot live "in sudocode" because sudowork talks to backends sudocode knows nothing about. SSOT must be **per-backend**, mediated by the ACP capability mechanism.

### Cap matrix (today)

```
Sudowork client (src/common/imageUtils.ts):
  IMAGE_TARGET_RAW_SIZE = 512 KB                     ← default
  LOW_IMAGE_TARGET_PATTERN /gemini|claude/i = 128 KB ← override for Claude/Gemini family
  IMAGE_MAX_WIDTH/HEIGHT = 1280 × 1280
  IMAGE_MAX_BASE64_SIZE  = 5 MB                      ← hard-limit fallback

Sudocode runtime (rust/crates/runtime/src/image_registry.rs):
  MAX_IMAGE_BYTES        = 5 MB
  MAX_IMAGE_DIMENSION    = 8000 × 8000

Claude Code, Codex, Qwen: each has its own opaque-to-us caps.
```

### Three remaining user-facing surfaces

1. **Wrong-model arm** (sudowork-side `AcpAgent.ts:988`): "当前模型 X 不支持图片分析". Only scode today; other backends handle internally.
2. **ImageTooLargeError fallback** (sudocode PR-E): "[Image #N was too large to send]" text placeholder. Rare pathological inputs only.
3. **Sticky processing indicator** if the upstream resize errors silently and the request hangs at transport. Theoretical today; not observed.

## Architecture decisions

### Decision 1 — Cap SSOT: ACP capability negotiation (per-backend)

Each ACP backend advertises its own preflight capability at session init. Sudowork queries; never hardcodes.

```
SessionInitialize response (ACP, extended via meta):
  {
    ...,
    "_meta": {
      "imageCapability": {
        "maxBytes": 5242880,                  // 5 MB — per this backend's preflight
        "maxDimension": 8000,                 // 8000 px
        "downsampleTargetBytes": 524288,      // 512 KB — backend's recommended target
        "autoHandlesOversized": true,         // backend will downsample on receive
        "autoHandlesWrongModel": true         // backend will VLM-route for non-vision models
      }
    }
  }
```

- **sudocode**: implements + advertises `{maxBytes: 5MB, maxDim: 8000, downsampleTarget: 512KB, autoHandlesOversized: true, autoHandlesWrongModel: true}`.
- **Claude Code / Codex / Qwen**: do not advertise this `_meta` (standard ACP only). Sudowork treats absence as "backend doesn't handle these — sudowork must compensate".

**Why this satisfies SSOT**: each backend is its own authority on its caps + behaviour. Sudowork is a faithful client. No cross-repo hardcoded constants that drift.

**Why ACP `_meta` (not a new method)**: ACP spec discourages new methods for non-spec extensions. `_meta` field on initialize response is the canonical extension point.

### Decision 2 — VLM-route logic in BOTH sudocode and sudowork (but only one fires per turn)

- **Sudocode** (Rust, in `runtime` crate): new module `vlm_describe.rs`. When `push_images` decides not to ship the image as `ContentBlock::Image` (because: a) decoded image fails preflight at q25, or b) active model isn't vision-capable), it calls VLM endpoint → substitutes `ContentBlock::Text { "[Image #N: <description>]" }`. Sudocode owns its own behaviour; advertises `autoHandlesOversized=true`, `autoHandlesWrongModel=true`.
- **Sudowork** (TS, sudowork-side fallback): when active ACP backend advertises `autoHandlesWrongModel=false` (or omits the capability), sudowork keeps the existing `callChatCompletionsWithImage` flow but **promotes it to transparent fallback** — fires automatically when chat-attach image hits a non-vision chat model, splices text into the user message, dispatches normally.

**Why both**:
- Sudocode-side keeps the fix close to the LLM call (smallest blast radius, smallest latency).
- Sudowork-side keeps Claude/Codex/Qwen users covered — those backends won't add this fix for us.
- Both reuse the same `callChatCompletionsWithImage`-style logic: VLM endpoint + credentials via existing sudorouter plumbing.

**The `_meta.autoHandles*` flags ensure only ONE fires per turn** — sudowork checks the advertised flag before falling back. No double-VLM-call.

### Decision 3 — Sudowork's hardcoded caps removed, replaced with ACP-advertised values

After Decision 1 lands:
- `IMAGE_TARGET_RAW_SIZE = 512 * 1024` → removed
- `LOW_IMAGE_TARGET_PATTERN` (128KB override) → removed
- `IMAGE_MAX_WIDTH/HEIGHT = 1280` → keep (this is sudowork's UX preference, not a transport cap; might also push to ACP later)
- Sudowork's `tryReadAsImage` queries the ACP session's advertised `downsampleTargetBytes` and `maxDimension`. Falls back to current defaults if backend doesn't advertise.

**Migration path**: this is backwards-compatible. Backends that don't advertise → sudowork uses today's defaults. Sudocode advertises → sudowork uses sudocode's values.

### Decision 4 — Keep dimension cap (1280 px) sudowork-side

Sudowork already constrains dim to 1280×1280 (`IMAGE_MAX_WIDTH/HEIGHT`). That's smaller than sudocode's 8000. After unification: sudocode advertises 8000 in capability; sudowork applies the tighter of its own 1280 default and sudocode's advertised value. Pre-existing client-side latency optimisation kept.

## Open questions for review

1. **Which VLM model for fallback?** Per-provider? Hardcoded gemini-flash / claude-haiku? Use the active conversation model if vision-capable, else fall back?
2. **Cost**: extra VLM round-trip per oversized/wrong-model image. ~0.5-2s latency + extra tokens charged. Worth it for the UX win?
3. **VLM quality loss**: text description loses colour, layout, fine text positioning. Acceptable for fallback path only (vs the current "user re-uploads smaller" pain)?
4. **Sudowork-side `LOW_IMAGE_TARGET_PATTERN` (128KB for Claude/Gemini)**: deliberate tightening from earlier commits (`1e52d407`, `938cbdd8`). Does the original empirical signal still hold after auto-VLM-route lands? If yes, sudocode should advertise `downsampleTargetBytes: 131072` for Claude/Gemini models. If no, drop the override.
5. **`_meta.imageCapability` schema**: agree on the field names + types here, or punt to a separate ACP-spec discussion?
6. **Claude Code / Codex / Qwen wrong-model handling**: the sudowork-side fallback is needed for them today. Long-term: file upstream issues asking them to handle internally?

## Implementation plan (this PR's commits)

1. **Design doc** (this file). Push first, get review.
2. **Sudocode capability**: add `imageCapability` to `SessionInitializeResponse._meta`. Cap constants moved to a `image_capability::Capability` struct exposed as `pub fn capability() -> Capability`. Reported in `initialize` handler.
3. **Sudocode VLM-route**: new `runtime/src/vlm_describe.rs`. Wire into `push_images`:
   - On `ImageTooLargeError`: call `describe_image_via_vlm` → substitute `ContentBlock::Text`.
   - On "active model not vision-capable" (read from session.current_model.vision_support): same path.
4. **Sudocode tests**: add integration test for capability advertisement + VLM-route fallback (mock VLM endpoint).
5. **Sudowork capability consumption**: in `AcpAgent` init, store `_meta.imageCapability` on session. In `tryReadAsImage`, query advertised values; fall back to today's defaults if absent.
6. **Sudowork wrong-model fallback**: replace `emitErrorMessage(tip); return {success: false}` at `AcpAgent.ts:988` with conditional VLM-route:
   - If `session.imageCapability.autoHandlesWrongModel === true`: hand off to backend (do nothing).
   - Else: call existing `callChatCompletionsWithImage` per image → splice `[Image #N: <description>]` into user message → continue.
7. **Sudowork tests**: unit tests for capability parsing + the fallback decision logic.
8. **E2E**: extend the existing `acp-session-preserved-after-error.yaml` with a vision-fallback case (paste image in non-vision conv → expect description spliced + normal reply).
9. **Roadmap update**: revise the `sudo-code-roadmap.html` row to "Shipped". Push to shareone post-merge.

Out of order: commits 2+5 (sudocode capability + sudowork consumption) form the cap-unification subset. Commits 3+6 (VLM-route both sides) form the wrong-model-fallback subset. Could be split into two PRs if reviewers prefer; current plan is one PR for atomic review.

## Test plan

| Layer | Test | Approach |
|---|---|---|
| Sudocode capability | Unit | `image_capability::capability()` returns documented values |
| Sudocode VLM-route | Integration | Mock VLM endpoint; assert text spliced when wrong-model OR ImageTooLargeError |
| Sudocode ACP init | PTY | Drive `scode acp` boot, assert `_meta.imageCapability` in initialize response |
| Sudowork capability consumption | Unit (vitest) | Parse various `_meta` shapes; fallback to defaults when absent |
| Sudowork wrong-model fallback | Integration | Mock ACP backend without `autoHandlesWrongModel` → assert sudowork VLM-route fires |
| End-to-end | YAML e2e (api) | Extend `acp-session-preserved-after-error.yaml` |

## Out of scope (followups)

- ACP spec PR to standardise `_meta.imageCapability` upstream — separate PR after the bilateral implementation proves the shape.
- Upstream issues against Claude Code / Codex / Qwen asking for auto-handling — file post-merge.
- VLM-route caching (same image asked twice → cached description) — premature optimisation.
- Multi-modal beyond images (audio, video) — orthogonal.

## Audit (8 principles)

1. **Simplify contract** ✅ — single SSOT mechanism (ACP capability) replaces 4+ hardcoded constants across two repos
2. **No boundary leak** ✅ — caps stay with the backend that enforces them; sudowork queries instead of guessing
3. **SSOT** ✅ — each backend = own truth; ACP `_meta` is the bridge
4. **DRY** ✅ — sudocode reuses its own `image_registry::preflight_base64`; sudowork reuses its own `callChatCompletionsWithImage`; only the routing decision is new
5. **Rust-first** ✅ — sudocode-side VLM logic in Rust (`vlm_describe.rs`)
6. **Perf-first** ✅ — VLM-route adds latency only when fallback path fires (oversized OR wrong-model); typical happy path unchanged
7. **Raft** ✅ N/A
8. **Systemic** ✅ — closes 3 user-facing surfaces simultaneously by addressing the underlying contract drift, not by adding a 5th cap layer

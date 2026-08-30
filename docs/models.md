# Models

`scode` is model-agnostic. This page describes the model aliases that
ship with `scode` and the provider-specific request handling.

## Aliases

Short names resolve to the current pinned versions:

| Alias | Resolves to | Provider |
|---|---|---|
| `opus` | `claude-opus-4-6` | Anthropic |
| `sonnet` | `claude-sonnet-4-6` | Anthropic |
| `haiku` | `claude-haiku-4-5` | Anthropic |
| `grok` | `grok-3` | xAI |

```bash
scode --model opus
scode --model sonnet --auth subscription
```

For the canonical live alias list, run `scode --help`.

> **These are convenience aliases, not the full model list.** `scode`
> routes through the backend (sudorouter), whose live catalog has 170+
> models - including Gemini (`gemini-3.5-flash`, ...), GPT-5, DeepSeek,
> GLM, Kimi, MiniMax, and more. Use any catalog model by its full name,
> e.g. `scode --model gemini-3.5-flash`.

## Provider-specific handling

Translating Claude-style messages to OpenAI-compatible chat completion
requests requires a few model-specific adjustments. Each rule below names
the model family and the request shape the family expects.

All detection strips a leading provider prefix (`dashscope/kimi-k2.5` →
`kimi-k2.5`) before matching.

### Kimi family — tool result field shape

Affected models: any model whose canonical name starts with `kimi-`
(case-insensitive — for example `kimi-k2.5`, `kimi-k1.5`, `kimi-moonshot`).

Behavior: the `is_error` field is omitted from tool result messages.
The Kimi backends accept tool results without this field.

### Reasoning models — sampling parameter shape

Affected model families:

- OpenAI: `o1*`, `o3*`, `o4*`
- xAI: `grok-3-mini`
- Alibaba DashScope: `qwen-qwq*`, `qwq*`, `qwen3-*-thinking`

Behavior: `temperature`, `top_p`, `frequency_penalty`, and
`presence_penalty` are stripped from requests. `reasoning_effort` is
included when explicitly set.

### GPT-5 family — completion token field name

Affected models: any model whose name starts with `gpt-5`.

Behavior: the request payload uses `max_completion_tokens` in place of
`max_tokens`.

### Qwen family — DashScope routing

Affected models: any model with a `qwen` prefix.

Behavior: requests route to the DashScope endpoint
`https://dashscope.aliyuncs.com/compatible-mode/v1`, authenticated via
`DASHSCOPE_API_KEY`. Some Qwen models also fall under the reasoning
family above and receive both treatments.

## Per-model extra request body — `extraBody`

Some backends gate behavior on a request-body field that no
Claude-shaped request carries. The canonical case: Alibaba DashScope
turns the Qwen3.x reasoning chain **on** by default and only
`"enable_thinking": false` in the body turns it off — so a "fast" model
spends its latency budget on a thinking pass nobody asked for.

Add the fields to the model entry in `sudocode.json`:

```jsonc
"models": {
  "qwen3.7-flash": {
    "alias": "qwen3.7-flash",
    "name": "qwen3.7-flash",
    "input": ["text"],
    "providers": {
      "api-key": {
        "provider": "dashscope",
        "model": "qwen3.7-flash",
        "api": "openai-completions"
      }
    },
    "extraBody": { "enable_thinking": false }
  }
}
```

- **Per model, not per process.** The setting hangs off the model entry,
  so one running `scode` can serve a fast model with thinking off and a
  deep model with thinking on, switching between them mid-session
  (`/model`, ACP `session/setModel`). A process-wide flag could not.
- **Values are arbitrary JSON** — bool, number (integer or float),
  string, array, object, `null` — and reach the wire verbatim.
- **Additive only.** A field `scode` computes itself is never
  overwritten: `model`, `messages`, `input`, `system`, `instructions`,
  `stream`, `stream_options`, `tools`, `tool_choice`, `max_tokens`,
  `max_completion_tokens`, `max_output_tokens`, plus any tuning
  parameter the caller explicitly set (`temperature`, `top_p`,
  `reasoning_effort`, …). Everything else is inserted as given. The list
  is `RESERVED_REQUEST_BODY_KEYS` in
  `rust/crates/api/src/types.rs`.
- **Absent means unchanged.** A model without `extraBody` sends exactly
  the payload it sent before this field existed.
- Applies to the `openai-completions` and `openai-responses` wire
  formats and to `anthropic-messages`. Codex and Gemini clients ignore
  it.

## Per-model token limits — `maxOutputTokens` / `contextWindow`

`scode` reads each model's context window and output-token ceiling from a
capabilities table that is **compiled into the binary** (refreshed from
sudorouter's `/v1/models` when available). A model the table has never
heard of inherits the table's `default` entry — and when the real provider
ceiling is lower, every request fails:

```
<400> InternalError.Algo.InvalidParameter: Range of max_tokens should be [1, 32768]
```

Two optional fields on the model entry override the table:

```jsonc
"qwen-flash": {
  "alias": "qwen-flash",
  "name": "qwen-flash",
  "input": ["text"],
  "providers": {
    "api-key": { "provider": "bailian", "model": "qwen-flash", "api": "openai-completions" }
  },
  "maxOutputTokens": 32768,
  "contextWindow": 1000000,
  "extraBody": { "enable_thinking": false }
}
```

- `maxOutputTokens` becomes the request's `max_tokens` for this model, taken
  as written — the usual heuristic cap (32k for `opus`, 64k otherwise) does
  not clamp it.
- `contextWindow` feeds the context meter, the auto-compaction threshold and
  the local preflight check.
- Both are keyed by the **wire model ID** the entry maps to, so the override
  reaches every code path that asks the capabilities table — main loop,
  subagents, preflight — not just the request builder.
- Absent means unchanged: the compiled table's numbers apply, exactly as
  before.

### `reasoning_effort` values

`--reasoning-effort` accepts `none`, `minimal`, `low`, `medium`, `high`.
`none` / `minimal` are part of OpenAI's own ladder and are what several
OpenAI-compatible backends accept to switch reasoning off; on DashScope
they are the only accepted values that do (`low` still reasons). The
flag reaches the wire on the `acp` and REPL paths; `--print` does not
send it. For a backend that wants a body field instead, use `extraBody`.

## Adding a model

To add a new model that requires special handling:

1. Identify which families above the model belongs to.
2. Extend the matching detection function in
   `rust/crates/api/src/providers/openai_compat.rs`.
3. Add a unit test for the detection alongside the existing tests.
4. Add an entry to the relevant section above.

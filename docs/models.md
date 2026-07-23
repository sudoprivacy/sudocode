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

## Adding a model

To add a new model that requires special handling:

1. Identify which families above the model belongs to.
2. Extend the matching detection function in
   `rust/crates/api/src/providers/openai_compat.rs`.
3. Add a unit test for the detection alongside the existing tests.
4. Add an entry to the relevant section above.

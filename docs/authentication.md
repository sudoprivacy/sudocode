# Authentication

`scode` supports three authentication modes. Select one with `--auth`, or
let auto-detection pick in order: `subscription` → `proxy` → `api-key`.

```bash
scode --auth subscription     # CLAUDE_CODE_OAUTH_TOKEN
scode --auth proxy            # PROXY_AUTH_TOKEN + PROXY_BASE_URL
scode --auth api-key          # ANTHROPIC_API_KEY, OPENAI_API_KEY, ...
```

## Modes

| Mode | Environment | Endpoint |
|---|---|---|
| `subscription` | `CLAUDE_CODE_OAUTH_TOKEN` | `api.anthropic.com` |
| `proxy` | `PROXY_AUTH_TOKEN` + `PROXY_BASE_URL` | `PROXY_BASE_URL` |
| `api-key` | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `XAI_API_KEY`, `GEMINI_API_KEY`, `DASHSCOPE_API_KEY` | Provider default |

## Subscription tokens

Generate a Claude subscription OAuth token with:

```bash
claude setup-token
```

Then export it as `CLAUDE_CODE_OAUTH_TOKEN` before running `scode`.

## Proxy mode

Proxy mode routes every provider call through a single URL with a single
bearer token. The proxy receives the original request shape and is
responsible for backend selection and rewriting.

```bash
export PROXY_BASE_URL="https://your-proxy.example.com"
export PROXY_AUTH_TOKEN="your-token"
scode --auth proxy
```

A reference deterministic proxy ships in the workspace as
`mock-anthropic-service` and is documented in [`mock-parity-harness.md`](./mock-parity-harness.md).

## Verifying credentials

```bash
scode doctor
```

`scode doctor` reports the resolved auth mode, the environment variables it
sees, and whether the resolved endpoint responds to a credential probe.

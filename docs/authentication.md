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

For config-file-based **named accounts** (instead of the `PROXY_*` env vars) and
per-project switching, see [Named accounts](#named-accounts--per-project-selection).

## Named accounts & per-project selection

Instead of the `PROXY_*` environment variables, you can define **named proxy
accounts** in config and switch between them per project — handy when different
repos need different backends or keys.

### Define accounts (global, once)

In `~/.nexus/sudocode/sudocode.json`, every key under `auth_modes.proxy` is an
account name mapping to `{ baseUrl, apiKey }`:

```json
{
  "auth_modes": {
    "proxy": {
      "default": { "baseUrl": "https://gw.example.com/v1",  "apiKey": "sk-..." },
      "clientA": { "baseUrl": "https://alt.example.com/v1", "apiKey": "sk-..." }
    }
  }
}
```

### Select one with `auth_profile`

`auth_profile` picks the active account. It resolves through the config layers
(later layers override earlier ones):

| Scope | File | Use for |
|---|---|---|
| Global default | `~/.nexus/sudocode/settings.json` | the account most repos use |
| Per-project (shared) | `<repo>/.nexus/sudocode/settings.json` | commit it so the whole team uses it |
| Per-project (local) | `<repo>/.nexus/sudocode/settings.local.json` | machine-only (gitignored) |

```json
{ "auth_profile": "clientA" }
```

So `cd <repo> && scode` automatically uses that repo's account — no flags. With
no `auth_profile` set, scode falls back to the first account.

Keys never leave the global `sudocode.json`: a per-project file references only
an account **name**, so a committed `settings.json` carries no secrets — each
developer defines the same-named account in their own global config. Confirm the
resolved account with `scode doctor` (see [Verifying credentials](#verifying-credentials)).

> Env vars still win: if `PROXY_AUTH_TOKEN` / `PROXY_BASE_URL` are set (see
> [Proxy mode](#proxy-mode)) they override any `auth_profile` selection.

## Verifying credentials

```bash
scode doctor
```

`scode doctor` reports the resolved auth mode, the environment variables it
sees, and whether the resolved endpoint responds to a credential probe. When a
named account is selected (see [Named accounts](#named-accounts--per-project-selection)),
it also prints an **Account** line, e.g.
`account=clientA base_url=https://alt.example.com/v1 auth_profile=clientA`.

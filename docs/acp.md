# Agent Communication Protocol

`scode` speaks the **Agent Communication Protocol (ACP)** natively, in two
transports that share a single handler chain.

## Transports

```bash
# stdio — for editors, IDE plugins, and CLI orchestrators
scode acp

# WebSocket + embedded Web UI — for browsers and service backends
scode acp serve --port 8080
```

`scode acp serve --port 8080` exposes:

- JSON-RPC over WebSocket at `ws://localhost:8080/ws`
- An interactive Web UI at `http://localhost:8080/`

Both transports share streaming, tool use, elicitation, and permission
prompting.

## Use cases

- **Editor plugins (Zed, VS Code, JetBrains)** speak ACP over stdio.
- **Web apps and dashboards** connect to the WebSocket endpoint or point
  a browser at `/` to use the embedded UI.
- **Automation pipelines and microservices** run `scode acp serve` as a
  long-lived process behind a load balancer.
- **Sub-agents and orchestrators** fan out work to multiple `scode`
  instances over the wire.

## Binding

For local-only use, bind to `127.0.0.1`. For team access, expose the port
behind your own auth proxy.

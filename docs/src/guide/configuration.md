# Configuration

wsPrism reads one YAML file (`--config`, `WSPRISM_CONFIG`, default
`./wsprism.yaml`). Unknown fields are rejected and `--check-config`
validates a file without serving, so mistakes surface before deployment.

The configuration is documented in one place, kept in sync with the code by
tests:

- **[wsprism.yaml](https://github.com/AsyncNomad/wsPrism/blob/main/wsprism.yaml)** — the annotated reference config: every
  option with its meaning and default (it is also tested to start a gateway).
- **[docs/CONFIG.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/CONFIG.md)** — topics in depth: authentication
  (tickets, JWT, connect proxy, plugins), the server API and internal listener,
  the RPC proxy, rooms, Hot lane pacing and echo, presence events, allowlist rules.

| Section | What it controls |
|---|---|
| `gateway` | listeners (public WebSocket, internal API), keep-alive, timeouts, draining, handshake rate limits, trusted proxies |
| `tenants[].limits` | frame size, sessions per tenant, rooms (total, per user, users per room) |
| `tenants[].policy` | allowlists, rate limits, session mode, browser origins, ticket sources, Hot lane behaviour, presence events |
| `tenants[].auth` | the authentication chain |
| `tenants[].api_keys` | server API keys (SHA-256) |
| `tenants[].rpc` | backend RPC routing |

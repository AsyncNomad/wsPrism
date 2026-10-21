# Configuration

wsPrism reads one YAML file (`--config`, `WSPRISM_CONFIG`, default
`./wsprism.yaml`). Unknown fields are rejected and `--check-config`
validates a file without serving, so mistakes surface before deployment.

The configuration is documented next to the code:

- **[wsprism.yaml](https://github.com/AsyncNomad/wsPrism/blob/main/wsprism.yaml)** — an annotated example: gateway
  settings, limits, policies and handshake limits (tests check that it
  builds a gateway).
- **[docs/CONFIG.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/CONFIG.md)** — the reference by topic: values
  from the environment (`${VAR}`), authentication (tickets, JWT, JWKS,
  connect proxy, plugins), the server API and internal listener, the RPC
  proxy, rooms, room history, the cluster broker, Hot lane pacing and echo,
  presence events, allowlist rules.

| Section | What it controls |
|---|---|
| `gateway` | listeners (public WebSocket, internal API), keep-alive, timeouts, draining, handshake rate limits, trusted proxies |
| `tenants[].limits` | frame size, sessions per tenant, rooms (total, per user, users per room) |
| `tenants[].policy` | allowlists, rate limits, session mode, browser origins, ticket sources, Hot lane behaviour, presence events |
| `tenants[].auth` | the authentication chain |
| `tenants[].api_keys` | server API keys (SHA-256) |
| `tenants[].rpc` | backend RPC routing |
| `tenants[].history` | rooms that keep history (size, age, bytes) for recovery on reconnect |
| `gateway.broker` | one node (`memory`, default) or a cluster on Redis (`redis`) |

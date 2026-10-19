# wsPrism Config (`wsprism.yaml`) v1

The gateway is configured with a single YAML file, passed with `--config`
(or `WSPRISM_CONFIG`; default `./wsprism.yaml`). Validate it without serving:

```bash
wsprism-gateway --check-config --config wsprism.yaml
```

The parser is **strict**: unknown fields and out-of-range values fail at
startup (and in `--check-config`), with the offending location.

The annotated reference is the repository's [`wsprism.yaml`](../wsprism.yaml);
every option is documented there.

## Minimal example

```yaml
version: 1
gateway:
  listen: "0.0.0.0:8080"
tenants:
  - id: "acme"
    limits:
      max_frame_bytes: 4096
```

## Reloading

There is **no hot reload yet**: configuration changes need a restart (drain
first; see `drain_grace_ms`). Hot reload of tenant policies is planned for
roadmap milestone M6.

---

## Rooms

Room names are 1-128 bytes of UTF-8 without control characters (WebSocket
clients and the server API alike). `limits.max_rooms_per_user` defaults to
**64** (`0` = unlimited): every joined room costs memory in several indices,
so an unbounded default would let one client grow it without limit.

---

## Allowlists must name existing services

Every `svc` in `policy.ext_allowlist` must be a built-in service (`room`,
`chat`), one registered with `GatewayBuilder::text_service`, or routed to a
backend by `tenants[].rpc`. Every service id in `policy.hot_allowlist` must be
the built-in relay (`1`) or registered with `GatewayBuilder::hot_service`.
Otherwise startup and `--check-config` fail with the offending rule
(development builds before the first release only warned).

---

## Hot Lane pacing (`tenants[].policy.hot_tick_hz`)

`0` (default): keyed Hot state is sent as soon as it arrives. `1..=240`: each
session's pending keyed state (latest value per sender/svc/key) is sent at
most once per tick, at a per-session phase within the period. Fewer flushes
when updates arrive faster than the tick, at the cost of up to one tick of
extra latency. Unkeyed frames are not paced.


**`policy.hot_echo_to_sender`** (default `true`): whether a session's own Hot
state comes back to it through the room relay. State-sync tenants usually set
`false`: each client already has its own state, and it saves about 1/N of a
room's Hot traffic. Services can do the same with `ctx.publish_room_others`.

---

## Authentication (`tenants[].auth`, ADR 0008)

An ordered list of authenticators. Each one either decides (admit / reject)
or passes a token it doesn't recognise to the next one.

```yaml
tenants:
  - id: acme
    auth:
      - kind: ticket     # one-time `wpt_...` tickets from POST /api/v1/tickets
      - kind: dev        # development only: `dev` -> user dev, `dev:<id>` -> user <id>
```

Whatever the authenticator (built-in or plugin), the user id it returns must
be 1-64 characters of `[A-Za-z0-9_-]`; anything else is rejected (and
logged), because user ids become parts of internal keys. The same rule
applies to the client's `?sid=` (HTTP 400).

Omitting `auth` means `[dev]` and logs a startup warning: anyone presenting
`dev` is admitted.

**Connect proxy** (`kind: proxy`): the backend decides. It accepts every
token, so put it **last** in the chain.

```yaml
      - kind: proxy
        url: "http://10.0.0.5:9000/wsprism/connect"
        timeout_ms: 2000        # default 2000 (1-60000)
        max_in_flight: 256      # concurrent calls; beyond it connections get 503
        secret_env: BACKEND_SECRET   # optional: sent as `Authorization: Bearer <value>`
```

The gateway sends `POST url {"tenant", "token", "client_ip"}`. The backend
answers `200 {"user": "alice", "permissions": {"subscribe": [...],
"publish": [...]}, "expires_at": <unix seconds>}` (`permissions` and
`expires_at` optional) to admit, `401`/`403` to reject (client gets 401).
Any other status, a timeout, an unreachable backend or a malformed reply is
**503 with `Retry-After`**, so an outage is never reported to clients as bad
credentials. The token is forwarded to the backend: use `https` outside a
trusted network.

**JWT** (`kind: jwt`): `sub` is the user id (`[A-Za-z0-9_-]{1,64}`), `exp`
is required, `nbf` is honoured. The algorithm is fixed per link — a token
whose header names another algorithm (or another `kid`, if `kid` is set)
is passed to the next link, never verified with a different algorithm.

```yaml
    auth:
      - kind: jwt
        algorithm: HS256          # HS256/384/512, RS256/384/512, PS256/384/512, ES256/384, EdDSA
        secret_file: /run/secrets/jwt   # HS*: or `secret_env: VAR`; never inline
        # public_key_file: /etc/wsprism/jwt.pem   # RS*/PS*/ES*/EdDSA: PEM public key
        issuer: "https://auth.example.com"   # optional; if set, `iss` is required
        audience: "wsprism"                  # optional; if set, `aud` is required
        leeway_s: 30                         # clock skew on exp/nbf (default 30, max 300)
        require_tenant: false                # true: tokens must carry wsprism.tenant
        # kid: "2026-10"                     # only tokens with this `kid` (key rotation)
```

wsPrism-specific claims sit under one object, so they never collide with an
issuer's own claims:

```json
{"sub": "alice", "exp": 1760000000,
 "wsprism": {"tenant": "acme", "subscribe": ["match:*"], "publish": ["match:42"]}}
```

A `wsprism.tenant` naming another tenant is always rejected. HMAC secrets
must be at least as long as the hash (32 / 48 / 64 bytes); a trailing
newline in `secret_file` is ignored. Keys are loaded at startup, so
`--check-config` reports a missing or malformed key. **The session is
closed when the token expires** (`exp` + `leeway_s`; v2 close 4001
`auth_expired`, v1 1008), checked once a second. There is no in-band token
refresh yet: the client reconnects with a new token. To rotate keys, list
two `jwt` links with different `kid`s. JWKS URLs are not supported yet
(they need the HTTP client of the proxy step).

**One-time tickets** (`kind: ticket`): the backend calls
`POST /api/v1/tickets` (server API, below) and hands the ticket to the
client, which presents it like any other ticket (header, subprotocol, or
query if `ticket_sources` allows it). A ticket is bound to the tenant whose
API key issued it, is valid for `ttl_s` (default 30, max 300) and is
**consumed on first use** — including a failed use (wrong tenant), so a
leaked ticket cannot be retried. Because tickets are single-use and
short-lived, enabling the `query` source for browsers is a reasonable
trade-off (the long-lived credential never enters a URL). Tickets live in
process memory: a restart invalidates outstanding ones (single node; a
shared store comes with M4).

---

## Server API (`gateway.api`, `tenants[].api_keys`, ADR 0008 §3)

Backends publish to rooms, disconnect users and read presence over HTTP on a
**separate listener** (off unless configured). Bind it to loopback or an
internal network.

```yaml
gateway:
  api:
    listen: "127.0.0.1:8081"
tenants:
  - id: acme
    api_keys:
      # sha256 of the key, lowercase hex: `printf %s "$KEY" | sha256sum`
      - name: backend
        sha256: "<64 hex chars>"
```

Requests send `Authorization: Bearer <key>`; the key selects the tenant.

The same internal listener also serves `/metrics`, `/healthz` and `/readyz`
without an API key (for scrapers and probes). `/metrics` is **not** on the
public WebSocket port unless `gateway.public_metrics: true`; health checks
stay on both.

| Endpoint | Body | Result |
|---|---|---|
| `POST /api/v1/publish` | `{"room": "r", "data": {...}}` (Ext, reliable by default) or `{"room": "r", "hot": {"svc": 1, "op": 2, "key": 0, "data_b64": "..."}}` (Hot, lossy by default); optional `"qos": "reliable"\|"lossy"` | `{"recipients": n}` |
| `POST /api/v1/disconnect` | `{"user": "u", "sid": "optional", "reason": "optional"}` | `{"closed": n}` |
| `GET /api/v1/rooms/{room}/presence` | — | `{"room": "r", "users": [...]}` |
| `POST /api/v1/tickets` | `{"user": "u", "ttl_s": 30, "permissions": {"subscribe": ["match:*"], "publish": ["match:42"]}}` (`ttl_s`, `permissions` and each list optional; an omitted list = unrestricted, `[]` = nothing) | `{"ticket": "wpt_...", "expires_in_s": 30}`; 400 if the tenant's `auth` has no `ticket` |

---

## RPC proxy (`tenants[].rpc`, ADR 0008 §6)

Ext requests matching `allow` are answered by your backend, in any
language, instead of a built-in service.

```yaml
tenants:
  - id: acme
    rpc:
      url: "http://10.0.0.5:9000/wsprism/rpc"
      allow: ["game:*", "shop:buy"]   # svc:type / svc:*; built-in services (room, chat, sys) are refused
      timeout_ms: 5000                # default 5000 (1-60000)
      max_in_flight: 16               # pending calls per connection (default 16)
      max_in_flight_total: 1024       # pending calls for the whole tenant; more get RESOURCE_EXHAUSTED
      secret_env: BACKEND_SECRET      # optional: `Authorization: Bearer <value>`
      require_joined_room: true       # default: a request naming a room the session hasn't joined gets NOT_ALLOWED
    policy:
      ext_allowlist: ["room:*", "game:*", "shop:buy"]   # still required: allow routes, the allowlist admits
```

For each matching request the gateway sends
`POST url {"tenant", "user", "sid", "svc", "type", "room", "joined", "data"}`
and answers the client with the table below. `room` is what the client sent;
`joined` says whether the gateway verified the session is in it. By default
requests naming a room the session hasn't joined are not forwarded
(`NOT_ALLOWED`); with `require_joined_room: false` they are, with
`joined: false`, and **the backend must not trust that room**.

| Backend reply | Client receives |
|---|---|
| `200 {"data": <any>}` | `sys.reply` with that `data` (and `reply_to`) |
| `200 {"error": {"code": "NOT_FOUND", "msg": "..."}}` | `sys.error` with that code (`[A-Z0-9_]{1,32}`, else `RPC_ERROR`) and `msg` (≤ 512 bytes) |
| anything else / timeout / unreachable | `sys.error UNAVAILABLE` (the backend's body is never forwarded) |

The session never waits for the backend: the call runs on its own task and
other messages keep flowing. To push messages to rooms or users from the
backend, use the server API (`/api/v1/publish`).

**Backend HTTP client** (connect and RPC proxies): one pooled client per
process; it never follows redirects (a token must not reach a host the
config didn't name), ignores `HTTP(S)_PROXY` environment variables, reads at
most 1 MiB of a reply, and verifies `https` against the **operating
system's certificate store** (so a private CA installed on the host is
trusted; minimal container images need the `ca-certificates` package).


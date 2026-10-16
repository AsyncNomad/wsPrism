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

## Hot Lane pacing (`tenants[].policy.hot_tick_hz`)

`0` (default): keyed Hot state is sent as soon as it arrives. `1..=240`: each
session's pending keyed state (latest value per sender/svc/key) is sent at
most once per tick, at a per-session phase within the period. Fewer flushes
when updates arrive faster than the tick, at the cost of up to one tick of
extra latency. Unkeyed frames are not paced.

---

## Authentication (`tenants[].auth`, ADR 0008)

An ordered list of authenticators. Each one either decides (admit / reject)
or passes a token it doesn't recognise to the next one.

```yaml
tenants:
  - id: acme
    auth:
      - kind: ticket     # one-time `wpt_...` tickets from POST /api/v1/tickets
      - kind: dev        # development only: `dev` / `dev:<id>`
```

Omitting `auth` means `[dev]` and logs a startup warning: anyone presenting
`dev` is admitted. The backend connect proxy arrives in the next M2 step.

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

| Endpoint | Body | Result |
|---|---|---|
| `POST /api/v1/publish` | `{"room": "r", "data": {...}}` (Ext, reliable by default) or `{"room": "r", "hot": {"svc": 1, "op": 2, "key": 0, "data_b64": "..."}}` (Hot, lossy by default); optional `"qos": "reliable"\|"lossy"` | `{"recipients": n}` |
| `POST /api/v1/disconnect` | `{"user": "u", "sid": "optional", "reason": "optional"}` | `{"closed": n}` |
| `GET /api/v1/rooms/{room}/presence` | — | `{"room": "r", "users": [...]}` |
| `POST /api/v1/tickets` | `{"user": "u", "ttl_s": 30, "permissions": {"subscribe": ["match:*"], "publish": ["match:42"]}}` (`ttl_s`, `permissions` and each list optional; an omitted list = unrestricted, `[]` = nothing) | `{"ticket": "wpt_...", "expires_in_s": 30}`; 400 if the tenant's `auth` has no `ticket` |

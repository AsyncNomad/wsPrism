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

## Cluster broker (`gateway.broker`, ADR 0011) — experimental

Operations guide (failure behaviour, Redis requirements, capacity):
[CLUSTER.md](CLUSTER.md).

By default (`kind: memory`) rooms live in one gateway process. With Redis,
several gateway nodes share rooms: a message published on any node reaches
the room's members on every node.

```yaml
gateway:
  broker:
    kind: redis
    url_env: WSPRISM_REDIS_URL     # or url: "rediss://user:pass@redis:6380/0"
    prefix: wsp                    # keys and channels: wsp:...
    pubsub_connections: 4          # subscriber connections, 1-64
    slot_ttl_s: 30                 # node slot lifetime without heartbeat, 3-300
    outbox_capacity: 65536         # frames queued for Redis (all tenants)
    outbox_per_tenant: 16384       # ... and per tenant
    outbox_max_wait_ms: 1000       # refuse while the oldest queued frame waited this long
```

- **Startup:** the node connects and claims a *node slot* (one of 1,024)
  before it serves anyone; if Redis is unreachable it does not start. Member
  ids (Hot `sender`) carry the slot, so they are unique across nodes.
- **Ordering:** Ext messages go through Redis, so every node delivers a
  room's messages in the same order (local members included, at the cost of
  one Redis round trip). Hot state is delivered locally at once and also
  sent to the other nodes.
- **Redis trouble:** frames wait in the outbox and are retried in order;
  when the outbox (or a tenant's share) is full, or its oldest frame has
  waited `outbox_max_wait_ms`, Reliable publishes are refused with
  `UNAVAILABLE` (server API: 503 + `Retry-After`) and Lossy ones are
  dropped — past saturation the gateway fails fast instead of letting
  latency grow. If a node's subscription drops, it resubscribes and sends
  `sys.gap` to the affected rooms' members (their apps resync). Hot state
  keeps flowing between local members.
- **Metrics:** `wsprism_broker_outbox_refused_total`,
  `wsprism_broker_retries_total`, `wsprism_broker_failed_total`.
- **TLS:** `rediss://` (rustls, system roots).
- **History** (`tenants[].history`) works across nodes: Redis gives each
  kept message its offset (one Lua script stores and publishes it), so
  every node sees the same offsets, and a client can resume on any node.
  Per room the history takes `max_bytes` of Redis memory at most, plus two
  small keys that outlive the last message by 24 h (the position). If Redis
  loses them (restart without persistence, eviction), the room starts a new
  epoch and clients resync — prefer a Redis with `maxmemory-policy
  noeviction` (or `volatile-*`) for history.
- **One-time tickets** are stored in Redis (`SET NX PX`, redeemed with
  `GETDEL`): issued by any node's server API, accepted once on any node.
  Redis holds the ticket's SHA-256, never the ticket. If Redis is down,
  ticket logins get 503 + `Retry-After` and issuing answers 503 (fail
  closed).
- **Session queues:** frames from Redis arrive in batches (up to 256), so
  keep `gateway.session_queue_capacity` well above that (the default 1024
  is fine); a small queue closes healthy readers as slow consumers.
- **Least-privilege ACL** (verified by the cluster tests, including the
  commands the Lua scripts run; `wsp` = your prefix):

  ```
  ACL SETUSER wsprism on >PASSWORD ~wsp:* &wsp:* -@all +publish +subscribe +unsubscribe +set +get +getdel +del +pexpire +time +zadd +zcard +zrange +zrangebyscore +zremrangebyrank +zpopmin +hmget +hgetall +hset +hincrby +hdel +hexists +sadd +srem +smembers +eval +evalsha +script|load +client|setinfo
  ```

  (`client|setinfo` is optional: without it the client library's
  identification is refused and logged by Redis, nothing else.)
- **Sessions are cluster-wide** (ADR 0011 R4-1): a reconnect with the same
  `sid` replaces the old connection on whatever node it is, and
  `sessions.max_sessions_per_user` / `mode: single` hold across nodes
  exactly (one Redis script decides; concurrent connects cannot both win).
  The server API's `disconnect` reaches every node. A node that dies takes
  its sessions out of the count. If Redis is down, new connections are
  refused with 1013 (try again later); connected sessions stay.
- **Presence is cluster-wide** (ADR 0011 R4-2): `GET /rooms/{room}/presence`
  and the `sys.joined` snapshot list every node's members. When a node
  dies, the others notice within its `slot_ttl_s` plus a few seconds,
  remove its members and send `member_left` for them once (with presence
  events on). A node stopped gracefully is treated the same way for the
  sessions it could not clean up before exiting.
- **Self-repair** (ADR 0011 R5): every 15 s each node checks itself
  against Redis. If Redis lost its data (restart without persistence) or
  the node was cut off longer than `slot_ttl_s` and taken for dead, it puts
  its sessions and room memberships back — nobody is disconnected. A local
  session the table no longer has (replaced elsewhere while the command to
  close it was lost) is closed as `replaced`. If another node took this
  node's slot, `/readyz` answers 503: restart the node.
- **Per node:** `max_sessions_total` and the room limits
  (`max_rooms_total`, `max_users_per_room`, `max_rooms_per_user`).
- **Scope:** one Redis (or Sentinel-managed primary). Keys use Redis Cluster
  hash tags already (`wsp:r:{tenant::room}`), so Cluster support will not
  change them.

---

## Room history (`tenants[].history`, ADR 0011 §3)

Rooms matching a rule keep their recent messages so clients can resume after
a disconnect (see PROTOCOL.md "History and recovery"). The first matching
rule applies; rooms matching none keep nothing (the default).

```yaml
tenants:
  - id: acme
    history:
      - { rooms: "chat:*", size: 200, ttl_s: 600 }       # defaults: 200, 600 s
      - { rooms: "orders", size: 1000, ttl_s: 3600, max_bytes: 4194304 }
```

| Field | Default | Range | Meaning |
|---|---|---|---|
| `rooms` | — | room name or `prefix*` | Rooms the rule covers |
| `size` | 200 | 1-10000 | Messages kept per room |
| `ttl_s` | 600 | 1-86400 | Seconds a message is kept |
| `max_bytes` | 1048576 | 1 KiB-64 MiB | Bytes kept per room (oldest go first) |

Memory: up to `max_bytes` per history room, so budget
`max_bytes × rooms that match` (bounded by `limits.max_rooms_total`). A room
with no members and nothing kept is forgotten after `ttl_s`; its next message
starts a new epoch. With the Redis broker, Redis keeps the history (see
"Cluster broker").

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


**`policy.presence_events`** (default `false`): send `sys.member_joined` /
`sys.member_left` to room members and list current members in `sys.joined`
(PROTOCOL.md, ADR 0010). Off by default because every join and leave is
broadcast to the whole room.

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
      - kind: dev        # development only: `dev` -> user dev, `dev:<id>` / `dev.<id>` -> user <id> (browsers: `dev.<id>`)
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
is required, `nbf` is honoured. (`exp` / `nbf` must be integer seconds; fractional
NumericDates are rejected by the verifier.) The algorithm is fixed per link — a token
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

**JWKS** (IdPs such as Auth0, Cognito, Keycloak, Okta): instead of a key
file, give the issuer's JWKS URL — keys are picked by the token's `kid`:

```yaml
      - kind: jwt
        algorithm: RS256                      # pinned as always; JWKS keys of another alg are ignored
        jwks_url: "https://idp.example.com/.well-known/jwks.json"   # https (http only to loopback)
        jwks_refresh_s: 300                   # refresh when older (60-86400, default 300)
        jwks_min_refetch_s: 30                # unknown kid -> refetch at most this often (default 30)
        jwks_max_stale_s: 86400               # distrust keys older than this if refresh keeps failing (default 1 day)
        issuer: "https://idp.example.com/"
```

Keys are fetched on first use and refreshed in the background; an unknown
`kid` (the IdP just rotated) triggers one immediate refetch, rate-limited so
random `kid`s cannot hammer the IdP. A failed refresh keeps the last good
keys — for at most `jwks_max_stale_s`, after which tokens get 503 (a key the
IdP revoked must not stay valid through a JWKS outage). Until keys have been
loaded once, connections get 503 + `Retry-After`
(not 401). Only signing keys whose type fits the pinned algorithm are used;
symmetric (`oct`) keys from a URL never are.

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
| `POST /api/v1/publish` | `{"room": "r", "data": {...}}` (Ext, reliable by default) or `{"room": "r", "hot": {"svc": 1, "op": 2, "key": 0, "data_b64": "..."}}` (Hot, lossy by default); optional `"qos": "reliable"\|"lossy"` | `{"recipients": n}`; 503 + `Retry-After` if a cluster broker's queue is full (ADR 0011) |
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


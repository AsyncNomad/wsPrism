# Configuration

The gateway reads one YAML file, given with `--config` or `WSPRISM_CONFIG` (default `./wsprism.yaml`). This page lists every setting. The repository's [`wsprism.yaml`](https://github.com/AsyncNomad/wsPrism/blob/main/wsprism.yaml) is an annotated example of the common ones.

```bash
wsprism-gateway --config wsprism.yaml --check-config
```

The parser is strict. Unknown fields, values out of range and inconsistent combinations (an idle timeout shorter than the ping interval, an allowlist naming a service that does not exist) fail at startup with the location of the problem. `--check-config` runs exactly the same checks without serving.

## Structure

```yaml
version: 1          # required; the only version so far
gateway: { … }      # process-wide: listeners, timeouts, broker
tenants: [ … ]      # one entry per tenant
```

## Values from the environment

Any value can contain `${NAME}` (startup fails if `NAME` is unset or empty), `${NAME:-default}`, or `$${` for a literal `${`.

- Substitution happens after parsing, so a value can never alter the structure of the file. Keys are never substituted.
- A value that is exactly one placeholder becomes a number or boolean if its result reads as one; anything else stays a string.
- Inside `{ }` or `[ ]`, quote the placeholder (`"${QUEUE}"`). Quoting does not affect typing.
- For secrets, prefer the `*_env` settings, which read the variable where it is used.

## `gateway`

| Setting | Default | Range | Meaning |
|---|---|---|---|
| `listen` | `0.0.0.0:8080` | | Public WebSocket listener |
| `api.listen` | off | | Internal listener: server API, `/metrics`, probes. Bind to loopback or a private network |
| `public_metrics` | `false` | | Also serve `/metrics` on the public listener |
| `ping_interval_ms` | 20000 | 5000–120000 | WebSocket ping interval |
| `idle_timeout_ms` | 60000 | 10000–600000, > ping interval | Close a connection with no inbound activity for this long |
| `writer_send_timeout_ms` | 1500 | 50–60000 | Close a session whose socket write stalls this long |
| `drain_grace_ms` | 2000 | ≤ 600000 | Time allowed for a graceful stop |
| `session_queue_capacity` | 1024 | 1–1048576 | Outbound queue per session, in messages |
| `socket.tcp_nodelay` | `true` | | Disable Nagle. Keep it on: with Nagle and delayed ACKs, small frames wait tens of milliseconds |
| `trusted_proxies` | `[]` | IPs or CIDRs | Proxies whose `X-Forwarded-For` names the client |
| `log_identities` | `plain` | `plain`, `hashed`, `off` | How user ids, session ids and IPs appear in logs |
| `log_identity_key_env` | random per process | | Variable holding the HMAC key for `hashed` (≥ 16 bytes) |
| `broker` | `kind: memory` | | See [Cluster broker](#cluster-broker) |

### `gateway.handshake_limit`

Token buckets applied before the WebSocket upgrade. Refused attempts get 429 with `Retry-After`.

| Setting | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Turn off only for load tests from a single address |
| `global_rps` / `global_burst` | 100 / 200 | Attempts per second across all clients |
| `per_ip_rps` / `per_ip_burst` | 10 / 50 | Attempts per second per client address |
| `ipv6_prefix_len` | 64 (32–128) | IPv6 clients are grouped by this prefix |
| `max_ip_entries` | 50000 | Addresses tracked; beyond that only the global bucket applies |

## `tenants[]`

| Setting | Meaning |
|---|---|
| `id` | 1 to 64 characters of `[A-Za-z0-9_-]`, unique |
| `limits` | Resource limits, below |
| `policy` | Message and session policy, below |
| `auth` | Authentication chain. Omitted: the development authenticator, with a warning |
| `api_keys` | Server API keys: `{name, sha256}` |
| `rpc` | RPC proxy to your backend |
| `history` | Room history rules |

### `limits`

| Setting | Default | Meaning |
|---|---|---|
| `max_frame_bytes` | 4096 | Largest client frame, enforced in the WebSocket codec (close 1009) |
| `max_sessions_total` | 0 (unlimited) | Concurrent sessions for the tenant, per node |
| `max_rooms_total` | 0 (unlimited) | Rooms with at least one member, per node |
| `max_users_per_room` | 0 (unlimited) | Members per room, per node |
| `max_rooms_per_user` | 64 | Rooms one user may be in at once; 0 = unlimited |

### `policy`

| Setting | Default | Meaning |
|---|---|---|
| `ext_allowlist` | `["room:join", "room:leave"]` | Ext messages clients may send: `svc:type`, `svc:*` |
| `hot_allowlist` | `[]` (Hot lane off) | Hot frames clients may send: `svc:op`, `svc:*` |
| `rate_limit_rps` / `rate_limit_burst` | 200 / 400 | Inbound messages per second |
| `rate_limit_scope` | `connection` | `connection`, `tenant` (shared pool) or `both` |
| `sessions.mode` | `multi` | `single` requires `max_sessions_per_user: 1` |
| `sessions.max_sessions_per_user` | 4 | Concurrent sessions per user (across nodes in a cluster) |
| `sessions.on_exceed` | `deny` | `deny` the new session, or `kick_oldest` |
| `allowed_origins` | `[]` (no browsers) | Exact origins, `https://*.example.com`, or `*` |
| `ticket_sources` | `[header, subprotocol]` | Where clients may present credentials, in order; `query` is opt-in |
| `hot_error_mode` | `sys_error` | `silent` drops refused Hot frames without telling the client |
| `hot_requires_active_room` | `true` | Protocol v1: Hot frames need a joined room |
| `hot_echo_to_sender` | `true` | Return a session's own Hot frames to it |
| `hot_tick_hz` | 0 (immediate) | 1–240: flush keyed Hot state at most this often per session |
| `presence_events` | `false` | Send `member_joined` / `member_left` and member snapshots |

Every service named in an allowlist must exist: a built-in service (`room`, `chat`, Hot `1`), one registered with `Gateway::builder`, or one routed by `rpc.allow`.

### `auth[]`

Each entry has a `kind`.

**`dev`**: development only. Accepts `dev` (user `dev`) and `dev.<id>` or `dev:<id>` (user `<id>`).

**`ticket`**: one-time tickets issued by `POST /api/v1/tickets`. No options.

**`jwt`**:

| Setting | Default | Meaning |
|---|---|---|
| `algorithm` | required | `HS256/384/512`, `RS256/384/512`, `PS256/384/512`, `ES256/384`, `EdDSA` |
| `secret_file` / `secret_env` | | HMAC secret (HS*), at least the hash length |
| `public_key_file` | | PEM public key (RS*, PS*, ES*, EdDSA) |
| `jwks_url` | | JWKS endpoint, `https` (plain `http` only to loopback) |
| `jwks_refresh_s` | 300 (60–86400) | Background refresh interval |
| `jwks_min_refetch_s` | 30 (1–3600) | Minimum interval between refetches for unknown `kid`s |
| `jwks_max_stale_s` | 86400 (1–604800) | Stop trusting keys this old if refreshing keeps failing |
| `issuer` / `audience` | | When set, `iss` / `aud` are required and checked |
| `leeway_s` | 30 (≤ 300) | Clock skew allowed on `exp` and `nbf` |
| `require_tenant` | `false` | Require the `wsprism.tenant` claim |
| `kid` | | Consider only tokens with this key id |

**`proxy`**:

| Setting | Default | Meaning |
|---|---|---|
| `url` | required | Your connect endpoint |
| `timeout_ms` | 2000 (1–60000) | Per call |
| `max_in_flight` | 256 | Concurrent calls; beyond this, 503 |
| `secret_env` | | Sent as `Authorization: Bearer <value>` |

**`plugin`**: `name` (registered with `Gateway::builder().auth_plugin`) and `options` (passed to its factory).

### `api_keys[]`

`{ name: "backend", sha256: "<64 hex characters>" }`. Compute the hash with `printf %s "$KEY" | sha256sum`.

### `rpc`

| Setting | Default | Meaning |
|---|---|---|
| `url` | required | Your RPC endpoint |
| `allow` | required | `svc:type` / `svc:*` patterns forwarded to it; built-in services are refused |
| `timeout_ms` | 5000 (1–60000) | Per call |
| `max_in_flight` | 16 | Pending calls per connection; beyond, `RATE_LIMITED` |
| `max_in_flight_total` | 1024 | Pending calls per tenant; beyond, `RESOURCE_EXHAUSTED` |
| `secret_env` | | Sent as `Authorization: Bearer <value>` |
| `require_joined_room` | `true` | Refuse requests naming a room the session has not joined |

### `history[]`

| Setting | Default | Range | Meaning |
|---|---|---|---|
| `rooms` | required | name or `prefix*` | Rooms the rule covers; the first matching rule wins |
| `size` | 200 | 1–10000 | Messages kept per room |
| `ttl_s` | 600 | 1–86400 | Seconds a message is kept |
| `max_bytes` | 1048576 | 1024–67108864 | Bytes kept per room |

## Cluster broker

`gateway.broker` with `kind: redis` connects nodes through Redis. See [Running a Cluster](../guide/cluster.md).

| Setting | Default | Range | Meaning |
|---|---|---|---|
| `url` / `url_env` | one required | | `redis://` or `rediss://` URL, or the variable holding it |
| `prefix` | `wsp` | 1–64 of `[A-Za-z0-9_.-]` | Prefix of every key and channel; lets clusters share a Redis |
| `pubsub_connections` | 4 | 1–64 | Subscriber connections |
| `slot_ttl_s` | 30 | 3–300 | A node's slot expires this long after its last heartbeat |
| `outbox_capacity` | 65536 | | Frames queued for Redis, all tenants |
| `outbox_per_tenant` | 16384 | 1–capacity | Frames one tenant may have queued |
| `outbox_max_wait_ms` | 1000 | 10–60000 | Refuse new frames while the oldest has waited this long |
| `tls.ca_file` | system store | | CA for the Redis certificate |
| `tls.client_cert_file` / `tls.client_key_file` | | | Client certificate for mutual TLS |
| `sentinel.addresses` | | | Sentinel URLs (`rediss://` for TLS to the sentinels) |
| `sentinel.master_name` | | | The primary's name in Sentinel |
| `replica_ack.replicas` | 1 with Sentinel, else off | 0–16 | Replicas that must hold a ticket consumption before the login succeeds |
| `replica_ack.timeout_ms` | 500 | 1–10000 | Longest wait; past it the login fails with 503 |
| `admission.max_latency_ms` | 500 | 0 (off) or 10–60000 | Refuse new connections while Redis answers slower than this |
| `admission.max_pending_claims` | 10000 | 1–1000000 | Refuse new connections while this many session claims wait for Redis |

## Reloading

Configuration is read once at startup. To change it, restart the node; with several nodes, restart them one at a time.

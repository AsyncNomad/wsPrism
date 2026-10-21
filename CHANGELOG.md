# Changelog

All crates (`wsprism`, `wsprism-gateway`, `wsprism-redis`, `wsprism-engine`, `wsprism-protocol`)
are released together with the same version. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Compatibility policy: [docs/API.md](docs/API.md) — wire contracts only grow;
the Rust API may break in 0.x minor releases, always listed here.

## [Unreleased] — 0.1.0 (first release)

### Gateway
- Multi-tenant WebSocket gateway: per-tenant policy (Ext/Hot allowlists, rate
  limits, session limits, room limits), browser `Origin` checks, trusted
  proxies, handshake rate limiting, graceful draining, `/healthz`, `/readyz`,
  `/metrics`.
- Two lanes: **Ext** (JSON envelopes, request correlation with `seq` /
  `reply_to`) and **Hot** (binary frames; v2 frames name their room, carry the
  sender, and keyed frames are conflated per recipient — latest value wins —
  optionally paced with `hot_tick_hz`).
- Protocol v2 (`wsprism.v2.json`) alongside v1, with close codes whose range
  tells clients how to reconnect (4102 `unavailable` when a cluster store is
  down; v1 1013). The wire contract only grows from 0.1.0; the field
  conventions are in docs/PROTOCOL.md.
- Authentication chain per tenant: one-time tickets, JWT (HS/RS/PS/ES/EdDSA,
  pinned algorithm, `kid` rotation, tenant binding, permission claims,
  session closed on expiry), connect proxy to your backend, and custom
  plugins. Per-user room permissions (subscribe / publish).
- JWT keys from the issuer's JWKS URL (`jwks_url`), picked by `kid`, refreshed
  in the background and refetched on rotation.
- In-band token refresh (`sys.refresh`): renew an expiring credential without
  reconnecting; the SDK does it automatically at a random 70–90% of the
  lifetime.
- Server API on a separate listener (`/api/v1/publish`, `/disconnect`,
  `/tickets`, `/rooms/{room}/presence`) with SHA-256-hashed API keys.
- `/metrics` is served on the internal listener (`gateway.api.listen`), not
  on the public port, unless `gateway.public_metrics: true`.
- Room history and recovery (opt-in `tenants[].history`): messages carry an
  `offset`, `sys.joined` an `epoch` and `offset`, and `room.join` with
  `data.recover` replays what a client missed, in order, or answers
  `recovered: false`. Slow consumers closed by C1 resume the same way.
- Presence events (opt-in `policy.presence_events`): `sys.member_joined` /
  `sys.member_left` on every way in and out, and a member snapshot in `sys.joined`.
- RPC proxy: allowlisted Ext requests answered by your backend over HTTP,
  without blocking the session. Requests naming a room the session has not joined are refused
  by default (`rpc.require_joined_room`); the backend gets `joined`.
- Startup fails when an allowlist names a service that does not exist.
- Every authenticator's user id (including plugins) must be `[A-Za-z0-9_-]{1,64}`;
  the client `sid` follows the same rule (HTTP 400 otherwise). The dev
  authenticator accepts `dev` (user `dev`) and `dev:<id>` / `dev.<id>`
  (user `<id>`, no `user:` prefix; `dev.` works in a browser subprotocol).
- The handshake rate limiter is on by default; IPv6 clients are limited per /64
  (`handshake_limit.ipv6_prefix_len`).
- `policy.hot_echo_to_sender: false` relays Hot state to everyone but the sender;
  services get `RealtimeCtx::publish_room_others`.
- Room names are limited to 1-128 bytes without control characters; `max_rooms_per_user` defaults to 64.
- Cluster broker on Redis (**experimental**, `gateway.broker.kind: redis`,
  crate `wsprism-redis`, default gateway feature `redis`): nodes share rooms
  over Redis pub/sub, member ids carry a per-node slot, Hot state crosses
  nodes with each node's room id, and a dropped subscription is reported to
  members as `sys.gap`. Room history works across nodes (offsets from a
  Redis Lua script; resume on any node) and one-time tickets are shared
  (issued on any node, accepted once on any node). Sessions are
  cluster-wide: sid replacement, per-user limits (exact, one Redis script)
  and the server API's disconnect span nodes. Presence is cluster-wide
  (queries, join snapshots), and the members of a node that dies are
  announced as having left, once. Nodes repair themselves after Redis
  loses data or a node is cut off, refill history rooms after a dropped
  subscription, tell clients when a room's history restarted, and never
  apply a retried history publish twice. A Redis that comes back with an
  older state (restored snapshot, promoted replica; detected by its
  `run_id`) starts a new history epoch, so clients are told instead of
  silently dropping reused offsets. One Redis at one address (no Sentinel or
  Redis Cluster yet). A node whose slot another node took (it was cut off
  longer than `slot_ttl_s`) drains and stops with an error instead of
  serving with colliding member ids. Chaos-tested: node `kill -9`, rolling
  update, a node frozen past its slot TTL, Redis restart (empty and from an
  older snapshot), Redis pause, stop while Redis is slow (docs/CLUSTER.md). Least-privilege ACL in docs/CONFIG.md.
- Server API publish answers 503 + `Retry-After` when a cluster broker's queue
  is full.
- Operations: `--log-format json` (`WSPRISM_LOG_FORMAT`) writes one JSON
  object per line with the session's `trace_id`, tenant, user, sid and IP;
  backend URLs are logged without credentials or query values.
  `${VAR}` / `${VAR:-default}` in configuration values. `/metrics` has
  `# HELP` on every family; the dispatch histogram is
  `wsprism_dispatch_duration_seconds` (seconds, was `_micros`).
  `Gateway::metrics_text()` now includes the egress, Hot slot and broker
  counters, like `/metrics`. `gateway.log_identities: plain | hashed |
  off` decides how user ids, client sids and IPs appear in logs (hashed:
  HMAC-SHA256, key from `log_identity_key_env`); delivery warnings name the
  connection id, never the user. Guide: docs/OPERATIONS.md.
- Container image: `Dockerfile` (distroless, non-root, JSON logs) and a
  two-node compose example on Redis (`deploy/compose/`).
- The binary prints the cause of internal startup errors (e.g. a missing
  configuration file) instead of "internal error".
- Graceful stop hands over a cluster node's queued frames (publishes it
  accepted, members leaving) before exiting, and refuses server API
  publishes (503) while draining. Rolling updates are chaos-tested.

### Rust API
- `wsprism` facade with `Gateway::builder()`: add your own `TextService` /
  `HotService` and authenticator plugins; `prelude`.
- API levels: stable (facade), SPI (`wsprism_engine::spi`, unstable),
  internal. MSRV 1.95.
- Broker boundary (`wsprism_engine::broker`, **experimental** like the Redis
  broker that implements it; ADR 0011 R1). One node uses the inline `MemoryBroker`, which
  costs nothing. Member ids (Hot `sender`) are now a node slot plus a per-room
  counter, unique among a room's live members across a cluster.
- SPI `Member` has `skip_upto` / `skip_epoch` (callers building one pass `0` /
  `None`).
- The `wsprism` facade forwards the `redis` feature (default on):
  `default-features = false` builds without Redis.

### Clients
- JavaScript SDK (`sdk/js`, no dependencies, Node 22+ and browsers): tickets
  via subprotocol, request/reply, rooms with rejoin, Hot state with stale
  frame dropping, reconnection by close-code range, history rooms resumed
  on reconnect (`recovered: false` on the rejoined room = resync), a `gap`
  event (with `epoch` when a room's history restarted), presence events,
  automatic token refresh (`sys.refresh`) and a list of gateway URLs tried
  in turn on reconnect.
- Example app `examples/chat-and-state` (Node backend + SDK).

### Known limitations
- The Redis cluster broker is experimental: young code, chaos-tested only
  for the scenarios above. It is high availability with limited scaling:
  room history is bounded by one Redis core (~150k messages/s), and each
  node is bounded at ~200k publishes/s (docs/CLUSTER.md). Tenant-wide
  session and room limits are per node. A Redis restart (with or without
  persistence) starts a new history epoch in every history room.
- Embedders should build with `codegen-units = 1` like this workspace
  (fan-out micro-benchmark −9–10% otherwise; docs/API.md).

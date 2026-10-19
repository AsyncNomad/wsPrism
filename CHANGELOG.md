# Changelog

All crates (`wsprism`, `wsprism-gateway`, `wsprism-engine`, `wsprism-protocol`)
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
  tells clients how to reconnect.
- Authentication chain per tenant: one-time tickets, JWT (HS/RS/PS/ES/EdDSA,
  pinned algorithm, `kid` rotation, tenant binding, permission claims,
  session closed on expiry), connect proxy to your backend, and custom
  plugins. Per-user room permissions (subscribe / publish).
- Server API on a separate listener (`/api/v1/publish`, `/disconnect`,
  `/tickets`, `/rooms/{room}/presence`) with SHA-256-hashed API keys.
- `/metrics` is served on the internal listener (`gateway.api.listen`), not
  on the public port, unless `gateway.public_metrics: true`.
- RPC proxy: allowlisted Ext requests answered by your backend over HTTP,
  without blocking the session. Requests naming a room the session has not joined are refused
  by default (`rpc.require_joined_room`); the backend gets `joined`.
- Startup fails when an allowlist names a service that does not exist.
- Every authenticator's user id (including plugins) must be `[A-Za-z0-9_-]{1,64}`;
  the client `sid` follows the same rule (HTTP 400 otherwise). The dev
  authenticator's users are now `dev` / `<id>` (no `user:` prefix).
- Room names are limited to 1-128 bytes without control characters; `max_rooms_per_user` defaults to 64.

### Rust API
- `wsprism` facade with `Gateway::builder()`: add your own `TextService` /
  `HotService` and authenticator plugins; `prelude`.
- API levels: stable (facade), SPI (`wsprism_engine::spi`, unstable),
  internal. MSRV 1.95.

### Clients
- JavaScript SDK (`sdk/js`, no dependencies, Node 22+ and browsers): tickets
  via subprotocol, request/reply, rooms with rejoin, Hot state with stale
  frame dropping, reconnection by close-code range.
- Example app `examples/chat-and-state` (Node backend + SDK).

### Known limitations
- Single node (no shared room state across gateways yet).
- Fan-out micro-benchmark is 8–12% slower than before the crate split until
  the release profile is tuned (ADR 0009 §6).
- No in-band token refresh: clients reconnect with a new credential (4001).

# wsPrism Protocol v1 (Draft)

wsPrism is a **Self-hosted Realtime Gateway** with a **Two-Lane** architecture:

- **Hot Lane (Binary)**: deterministic, ultra-low latency, **native-only**
- **Ext Lane (Text/JSON)**: flexible, customizable via **WASM plugins (ingress-only)**

Key contracts:
- Hot Lane packets **do not include `room`**. They are routed to the user's **active room**.
- Ingress pipeline for Ext Lane: **Policy → Plugin → Service**

---

## 1) WebSocket Handshake

```
ws(s)://<gateway>/v1/ws?tenant=<TENANT_ID>[&sid=<SESSION_ID>]
```

The ticket is presented through one of the tenant's `policy.ticket_sources`
(tried in order; default `[header, subprotocol]`):

| Source | How | Use from |
|---|---|---|
| `header` | `Authorization: Bearer <ticket>` | Native apps, game engines, servers |
| `subprotocol` | `Sec-WebSocket-Protocol: wsprism.v1, wsprism.auth.<ticket>` | Browsers (cannot set headers on a WebSocket) |
| `query` | `?ticket=<ticket>` | Legacy only. Off by default: URLs are written to proxy/access logs |

Subprotocol tokens must be HTTP token characters (base64url without padding
and JWTs qualify).

**Protocol version.** If the client offers subprotocols, the server selects
the first supported version in its preference order (`wsprism.v2.json`, then
`wsprism.v1`) and echoes only that; the `wsprism.auth.*`
entry is never echoed. Offering subprotocols without a supported version is
rejected with HTTP 400. No `Sec-WebSocket-Protocol` header implies v1.

Server (on success):

```json
{"v":1,"svc":"sys","type":"authed","data":{"tenant":"<id>","user":"<user>","sid":"<sid>"},"trace_id":"<id>"}
```

---

## 2) Ext Lane: Text Envelope (JSON)

### Schema

```json
{
  "v": 1,
  "svc": "chat",
  "type": "send",
  "flags": 0,
  "seq": 123,
  "room": "party:1",
  "data": { ... }
}
```

### Flags (u32)
- `0x01`: SEQ_PRESENT
- `0x02`: ROOM_PRESENT
- `0x04`: ACK_REQUESTED

`data` is stored as RawValue in the core and parsed by services.

### Request correlation

If a request carries `seq`, every response the gateway or a service sends
**while handling that request** (success like `sys.joined`, or `sys.error`,
including a policy close notice) includes `"reply_to": <seq>`. Responses to
requests without `seq` have no `reply_to`. Server-initiated messages (room
broadcasts, idle timeout, kicks) never carry `reply_to`. Hot Lane frames with
`SEQ_PRESENT` are correlated the same way (their `sys.error` replies carry the
frame's `seq`).

Services answer via `RealtimeCtx::reply`, which adds `reply_to` automatically.

---

## 3) Hot Lane: Binary Frame

Little-endian.

```
[ v:u8=1 ]
[ svc_id:u8 ]
[ opcode:u8 ]
[ flags:u8 ]
[ seq?:u32 ]     // flags & 0x01
[ payload... ]   // opaque
```

### Flags (u8)
- `0x01`: SEQ_PRESENT
- `0x02`: ACK_REQUESTED

Hot Lane routing:
- `svc_id` routes to a native BinaryService
- **room is resolved by presence.active_room**

**Delivery today (WebSocket).** Hot Lane traffic is *lossy only inside the
gateway*: when a recipient's outbound queue is full, the frame is dropped for
that recipient. On the network it still rides TCP, so a lost packet delays all
later frames until it is retransmitted (head-of-line blocking). Truly
unreliable, unordered delivery (WebTransport datagrams) is the planned next
transport — see `docs/design/hot-lane-over-webtransport.md`.

---

### Protocol v2 Hot frames (ADR 0006 §3, R7)

Sessions that negotiated `wsprism.v2.json` send and receive Hot frames that
name their target room explicitly; there is no implicit active room.

```
client -> server: [kind=0x01][flags][chan u32 LE][svc u8][op u8][key u32 LE?][seq u32 LE?][payload]
server -> client: [kind=0x01][flags][chan u32 LE][svc u8][op u8][key u32 LE?][seq u32 LE?][sender u32 LE][payload]
flags: 0x01 KEY · 0x02 SEQ · 0x04 SENDER (server only; a client setting it, or any unknown bit, is a decode error)
```

- `chan` is the **room id** from the v2 `sys.joined` reply (`"chan"`), the
  same for every member of the room. A frame naming a room this session has
  not joined is dropped (`policy_decisions{reason="unknown_channel"}`; with
  `hot_error_mode: sys_error` the client also gets `sys.error NOT_ALLOWED`).
- `sender` is the publisher's `member_id` (also in `sys.joined`), new for
  every join. Receivers drop frames whose `seq` is older than the last one
  seen for the same `(sender, key)` (datagrams can be reordered).
- v1 sessions in the same room keep receiving the payload only.
- **Keyed frames are conflated per recipient**: a frame with `key` is state,
  and a recipient that falls behind gets the newest value per
  `(sender, svc, key)` rather than every stale one. Frames without `key`
  are delivered individually (and dropped if the recipient's queue is full).

### Server publishes

Messages a backend publishes through the server API arrive as
`{"v":1,"svc":"server","type":"publish","room":"<room>","data":{...}}` (Ext)
or as a Hot frame from sender `0` (v1: payload only).

### RPC replies

When the tenant routes a `svc:type` to its backend (`tenants[].rpc`), the
answer is `{"v":1,"svc":"sys","type":"reply","reply_to":<seq>,"data":...}`
(or a `sys.error`). Send a `seq` to correlate: backend answers can arrive
out of order with respect to other replies. More than `max_in_flight`
pending calls on one connection are answered `RATE_LIMITED` immediately.

## 4) Ping/Pong & Idle timeout
Gateway periodically pings; client must pong. Idle connections are closed.

---

## 5) Rooms

- `{"svc":"room","type":"join","room":"<id>"}` → `sys.joined` (`room`). The joined room becomes the session's **active room** (Hot Lane target).
- `{"svc":"room","type":"leave","room":"<id>"}` → `sys.left` (`room`). Without `room`, leaves the active room. Leaving the active room clears it. Leaving a room the session hasn't joined → `sys.error BAD_REQUEST`.
- `chat.send` requires the sending session to have joined the target room (`NOT_ALLOWED` otherwise).

---

### Token refresh (ADR 0012)

When the credential expires (JWT `exp`, connect-proxy `expires_at`),
`sys.authed` carries `"expires_in_s"`. Before then, send a fresh token
in-band instead of reconnecting:

```json
{"v":1,"svc":"sys","type":"refresh","seq":7,"data":{"token":"<new token>"}}
{"v":1,"svc":"sys","type":"refreshed","reply_to":7,"data":{"expires_in_s":900}}
```

The token goes through the tenant's authentication chain like a new
connection. It must be for the **same user** (else `AUTH_FAILED`). The new
token's room permissions replace the old ones at once: joined rooms it no
longer allows are left (`sys.left`). On failure (`AUTH_FAILED`, or
`UNAVAILABLE` when the auth backend is down) the session continues until the
old expiry, then closes with 4001 (no grace period, even while a refresh is
in flight). A new token with an *earlier* expiry is accepted as is. One
refresh at a time and at most one per second per connection (`RATE_LIMITED`).
`sys.refresh` is not subject to the tenant allowlist; the connection rate
limit applies.

### Presence events (ADR 0010)

Tenants with `policy.presence_events: true` tell room members about
membership changes (other members only; Reliable):

```json
{"v":1,"svc":"sys","type":"member_joined","room":"lobby","data":{"user":"bob","member_id":7}}
{"v":1,"svc":"sys","type":"member_left","room":"lobby","data":{"user":"bob","member_id":7}}
```

Every way out sends `member_left`: `room.leave`, disconnect, eviction (sid
reuse, session limit), and the server API's disconnect. `member_id` is the
`sender` of that member's v2 Hot frames. Their `sys.joined` also lists who
is already there: `"members": [{"user":"alice","member_id":3}]` (at most
1000; `"members_truncated": true` if more). A repeated join of the same room
sends nothing.

## 6) Error codes

`sys.error` carries `data.code` from this fixed set (`ClientCode` in
`wsprism-protocol`) and a human-readable `data.msg`. Clients should branch on
`code`, never on `msg`. Internal errors never expose their cause.

| Code | Meaning |
|---|---|
| `BAD_REQUEST` | Malformed or invalid request (details in `msg`) |
| `AUTH_FAILED` | Authentication failed |
| `RATE_LIMITED` | Rate limited |
| `PAYLOAD_TOO_LARGE` | Payload too large |
| `NOT_ALLOWED` | Not permitted by policy: `svc:type` outside the tenant's `ext_allowlist`, a room outside the user's permissions, `chat.send` to a room not joined |
| `RESOURCE_EXHAUSTED` | A tenant/room/user limit was reached |
| `UNSUPPORTED_VERSION` | Unsupported protocol version |
| `TOO_MANY_SESSIONS` | Per-user session limit reached (`on_exceed: deny`) |
| `SLOW_CONSUMER` | The client could not keep up with its outbound queue |
| `TIMEOUT` | No activity within the idle timeout |
| `UNAVAILABLE` | A backend the request depends on (RPC proxy) failed or timed out; retry later |
| `INTERNAL` | Internal error (`msg` is always `internal error`) |

Requests answered by an **RPC backend** (`tenants[].rpc`) may also carry the
backend's own application code (`[A-Z0-9_]{1,32}`, e.g. `NOT_FOUND`); an
invalid one becomes `RPC_ERROR`.

---

## 7) Rejections & close codes

Before the upgrade (plain HTTP responses):

| HTTP | When |
|---|---|
| 400 | Unknown tenant |
| 400 | Subprotocols offered but no supported version (`wsprism.v2.json`, `wsprism.v1`) |
| 401 | Missing ticket (none of the tenant's `ticket_sources` present) or invalid ticket |
| 403 | Browser `Origin` not in the tenant's `policy.allowed_origins` (requests without `Origin` are not checked) |
| 429 + `Retry-After` | Handshake rate limit |
| 503 (+ `Retry-After`) | Draining, or tenant session capacity reached |

After the upgrade, the gateway sends an explanatory `sys.*` message (when
there is one) and then a close frame:

| Close code | Reason string | Preceded by | When |
|---|---|---|---|
| 1000 | `idle_timeout` | `sys.error TIMEOUT` | No inbound activity within `idle_timeout_ms` |
| 1001 | `draining` | — | Gateway shutting down |
| 1002 | `decode_error` | `sys.error BAD_REQUEST` / `UNSUPPORTED_VERSION` | Malformed envelope or hot frame |
| 1008 | `policy` | `sys.error` (Hot Lane: only in `sys_error` mode) | Policy decided to close |
| 1008 | `too_many_sessions` | `sys.error TOO_MANY_SESSIONS` | Per-user session limit with `on_exceed: deny` |
| 1008 | `max_sessions_exceeded` | `sys.kicked` | Evicted by a newer session (`on_exceed: kick_oldest`) |
| 1008 | `replaced` | `sys.kicked` | Another connection reused this `sid` |
| 1008 | `slow_consumer` | `sys.error SLOW_CONSUMER` (best effort) | Outbound queue full when replying to the client's request, or when a reliable message (e.g. `chat`) didn't fit. Reconnect and resync. |
| 1009 | `message too big` | — | Message exceeded the tenant `max_frame_bytes` |
| 1008 | `disconnected` | `sys.kicked{reason}` | The tenant's backend disconnected the user (server API) |
| 1013 | `capacity` | `sys.error RESOURCE_EXHAUSTED` | Tenant capacity reached during admission |

### Protocol v2 close codes (ADR 0006 §6)

Sessions that negotiated `wsprism.v2.json` get the same reasons and notices,
but 4000-range codes whose **range tells the client how to reconnect**:

| Range | Client should | Codes |
|---|---|---|
| 4000-4099 | **not reconnect unchanged** (new ticket / config needed) | 4001 `auth_expired` (the credential's `exp` passed: get a fresh token, then reconnect), 4003 `policy`, 4004 `too_many_sessions`, 4005 `replaced`, 4006 `max_sessions_exceeded`, 4007 `disconnected`, 4009 `message too big`, 4010 `decode_error` |
| 4100-4199 | reconnect **after backoff** (≥ 1 s, exponential + jitter) | 4100 `capacity`, 4101 `slow_consumer`, 4103 `draining` |
| 4200-4299 | reconnect **immediately** | 4200 `idle_timeout` |

Clients should treat a standard 1009 like 4009. (Planned: `sys.reconnect{after_ms}`
before 4103 to spread redeploy reconnects.)

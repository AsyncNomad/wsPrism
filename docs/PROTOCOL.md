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

**Protocol version.** If the client offers subprotocols, the server selects a
supported version (`wsprism.v1`) and echoes only that; the `wsprism.auth.*`
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

## 4) Ping/Pong & Idle timeout
Gateway periodically pings; client must pong. Idle connections are closed.

---

## 5) Rooms

- `{"svc":"room","type":"join","room":"<id>"}` → `sys.joined` (`room`). The joined room becomes the session's **active room** (Hot Lane target).
- `{"svc":"room","type":"leave","room":"<id>"}` → `sys.left` (`room`). Without `room`, leaves the active room. Leaving the active room clears it. Leaving a room the session hasn't joined → `sys.error BAD_REQUEST`.
- `chat.send` requires the sending session to have joined the target room (`NOT_ALLOWED` otherwise).

---

## 6) Error codes

`sys.error` carries `data.code` from this fixed set (`ClientCode` in
`wsprism-core`) and a human-readable `data.msg`. Clients should branch on
`code`, never on `msg`. Internal errors never expose their cause.

| Code | Meaning |
|---|---|
| `BAD_REQUEST` | Malformed or invalid request (details in `msg`) |
| `AUTH_FAILED` | Authentication failed |
| `RATE_LIMITED` | Rate limited |
| `PAYLOAD_TOO_LARGE` | Payload too large |
| `NOT_ALLOWED` | Not permitted by policy (e.g. `chat.send` to a room not joined) |
| `RESOURCE_EXHAUSTED` | A tenant/room/user limit was reached |
| `UNSUPPORTED_VERSION` | Unsupported protocol version |
| `TOO_MANY_SESSIONS` | Per-user session limit reached (`on_exceed: deny`) |
| `SLOW_CONSUMER` | The client could not keep up with its outbound queue |
| `TIMEOUT` | No activity within the idle timeout |
| `INTERNAL` | Internal error (`msg` is always `internal error`) |

---

## 7) Rejections & close codes

Before the upgrade (plain HTTP responses):

| HTTP | When |
|---|---|
| 400 | Unknown tenant |
| 400 | Subprotocols offered but no supported version (`wsprism.v1`) |
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
| 1013 | `capacity` | `sys.error RESOURCE_EXHAUSTED` | Tenant capacity reached during admission |

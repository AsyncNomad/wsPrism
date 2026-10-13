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
ws(s)://<gateway>/v1/ws?tenant=<TENANT_ID>&ticket=<TICKET>
```

Server (on success):

```json
{"v":1,"svc":"sys","type":"authed","flags":0,"data":{"user_id":"<string>"}}
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

---

## 4) Ping/Pong & Idle timeout
Gateway periodically pings; client must pong. Idle connections are closed.

---

## 5) Rooms

- `{"svc":"room","type":"join","room":"<id>"}` → `sys.joined` (`room`). The joined room becomes the session's **active room** (Hot Lane target).
- `{"svc":"room","type":"leave","room":"<id>"}` → `sys.left` (`room`). Without `room`, leaves the active room. Leaving the active room clears it. Leaving a room the session hasn't joined → `sys.error BAD_REQUEST`.
- `chat.send` requires the sending session to have joined the target room (`NOT_ALLOWED` otherwise).

---

## 6) Rejections & close codes

Before the upgrade (plain HTTP responses):

| HTTP | When |
|---|---|
| 400 | Unknown tenant |
| 401 | Invalid ticket |
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
| 1008 | `slow_consumer` | `sys.error SLOW_CONSUMER` (best effort) | Outbound queue full when replying to the client's request |
| 1009 | `message too big` | — | Message exceeded the tenant `max_frame_bytes` |
| 1013 | `capacity` | `sys.error RESOURCE_EXHAUSTED` | Tenant capacity reached during admission |

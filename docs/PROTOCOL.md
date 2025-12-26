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

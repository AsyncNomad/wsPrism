# Wire Protocol

This is the contract between clients and the gateway. If you use the JavaScript SDK you rarely need it. If you write a client for another platform, it is everything you need.

## Versions

Two versions are negotiated through the WebSocket subprotocol:

- **v2** (`wsprism.v2.json`): the current version and the SDK's default. Hot frames name their room explicitly, and close codes tell the client how to reconnect.
- **v1** (`wsprism.v1`): kept for older clients. Hot frames go to the session's *active room*, the last one joined.

The server picks the first version it supports in the order v2, v1, and echoes only that one. A client that offers subprotocols but no supported version is refused with 400. A client that offers none gets v1.

### Compatibility

From 0.1.0 on, the protocol only grows. New fields, message types and codes may appear; existing ones keep their names, types and meanings. **Clients must ignore fields they do not recognise.**

## Connecting

```
ws(s)://<host>/v1/ws?tenant=<tenant>[&sid=<session id>]
```

`sid` is optional, 1 to 64 characters of `[A-Za-z0-9_-]`. A new connection with the same `sid` replaces the old one, which is closed with `replaced`.

The credential goes where the tenant's `ticket_sources` allow:

| Source | Request |
|---|---|
| `header` | `Authorization: Bearer <ticket>` |
| `subprotocol` | `Sec-WebSocket-Protocol: wsprism.v2.json, wsprism.auth.<ticket>` |
| `query` | `?ticket=<ticket>` (off by default) |

Subprotocol tokens must consist of HTTP token characters; base64url without padding and JWTs qualify.

On success the first message is:

```json
{"v":1,"svc":"sys","type":"authed","data":{"tenant":"acme","user":"alice","sid":"tab-1"},"trace_id":"5f0c…"}
```

With an expiring credential, `data` also carries `expires_in_s`.

### Refusals before the upgrade

| HTTP | Cause |
|---|---|
| 400 | Unknown tenant, invalid `sid`, or no supported protocol version offered |
| 401 | Credential missing or rejected |
| 403 | Browser `Origin` not in `allowed_origins` |
| 429 + `Retry-After` | Handshake rate limit |
| 503 | The node is draining; connect to another (no `Retry-After`) |
| 503 + `Retry-After` | Tenant at capacity, authentication backend unavailable, or the cluster is busy |

## Ext lane: JSON envelopes

```json
{
  "v": 1,
  "svc": "chat",
  "type": "send",
  "seq": 123,
  "room": "party:1",
  "data": { }
}
```

| Field | Meaning |
|---|---|
| `v` | Envelope format version, always 1. Not the protocol version |
| `svc`, `type` | Service and message type; together they are what the allowlist checks |
| `seq` | Optional request number. Every response to this request carries `"reply_to": <seq>` |
| `room` | Target room, when the message concerns one |
| `data` | Payload, passed to the service as is |
| `flags` | Optional bit field: `0x01` seq present, `0x02` room present, `0x04` ack requested |

Responses produced while handling a request (`sys.joined`, `sys.reply`, `sys.error`, even a policy close notice) carry `reply_to`. Messages the client did not ask for, such as room broadcasts, presence events or kicks, never do.

### Field conventions

- The result of the client's own request travels at the top level (`room`, `chan`, `member_id`, `members`, `epoch`, `offset`, `recovered`). A payload, or an event about someone else, travels in `data`.
- `epoch`, `trace_id`, `chan` and `member_id` are opaque. Compare them and send them back; never parse them.
- `offset` is an integer that fits in a JavaScript number.

### Built-in messages

| Client sends | Gateway answers |
|---|---|
| `room.join {room, data?: {recover: {epoch, offset}}}` | `sys.joined {room, chan, member_id, members?, epoch?, offset?, recovered?}` |
| `room.leave {room?}` | `sys.left {room}`. Without `room`, leaves the active room |
| `chat.send {room, data}` | Broadcast to the room; the sender must have joined it |
| `sys.refresh {data: {token}}` | `sys.refreshed {data: {expires_in_s}}` |
| a request routed to your backend | `sys.reply {data}` |

| Gateway sends unprompted | Meaning |
|---|---|
| `server.publish {room, data}` | Published by your backend |
| `sys.member_joined` / `sys.member_left {room, data: {user, member_id}}` | Presence events, if enabled |
| `sys.gap {room, epoch?}` | Messages of the room may be missing; with `epoch`, its history restarted |
| `sys.kicked {data: {reason}}` | The session is about to be closed by the server |
| `sys.error {data: {code, msg}}` | An error, with `reply_to` if it answers a request |

In a history room every room message carries `offset`, and `room` is added if it was missing.

## Hot lane: binary frames

All integers are little-endian.

**v2, client to server:**

```
[kind=0x01][flags u8][chan u32][svc u8][op u8][key u32]?[seq u32]?[payload…]
```

**v2, server to client:**

```
[kind=0x01][flags u8][chan u32][svc u8][op u8][key u32]?[seq u32]?[sender u32][payload…]
```

| Flag | Field |
|---|---|
| `0x01` | `key` present |
| `0x02` | `seq` present |
| `0x04` | `sender` present (server only; a client setting it, or any unknown bit, is a decode error) |

- `chan` is the room id from `sys.joined`, the same for every member of the room on a node.
- `sender` is the publisher's `member_id`; frames published through the server API come from sender `0`.
- Frames with a `key` are conflated per recipient: a recipient that falls behind gets the newest frame per `(sender, svc, key)`.
- Receivers should drop a frame whose `seq` is older than the last one seen for the same sender and key.

**v1:**

```
[v=1][svc u8][op u8][flags u8][seq u32]?[payload…]      flags: 0x01 seq, 0x02 ack requested
```

v1 frames go to the active room, and v1 receivers get the payload only.

## Error codes

`sys.error` carries `data.code` from this fixed set and a human-readable `data.msg`. Branch on `code`, never on `msg`. Internal errors never reveal their cause.

| Code | Meaning |
|---|---|
| `BAD_REQUEST` | Malformed or invalid request |
| `AUTH_FAILED` | Authentication failed |
| `RATE_LIMITED` | Rate limited |
| `PAYLOAD_TOO_LARGE` | Payload too large |
| `NOT_ALLOWED` | Not permitted: outside the allowlist, outside the user's permissions, or a room not joined |
| `RESOURCE_EXHAUSTED` | A tenant, room or user limit was reached |
| `UNSUPPORTED_VERSION` | Unsupported protocol version |
| `TOO_MANY_SESSIONS` | Per-user session limit reached |
| `SLOW_CONSUMER` | The client could not keep up |
| `TIMEOUT` | No activity within the idle timeout |
| `UNAVAILABLE` | A dependency is unavailable or saturated (your backend, the cluster's Redis); retry later |
| `INTERNAL` | Internal error |

Requests answered by your backend through the RPC proxy may also carry the backend's own codes (`[A-Z0-9_]{1,32}`).

## Close codes

The gateway sends an explanatory `sys.*` message when there is one, then closes. Under v2 the code's range tells the client what to do next:

| Range | Client should |
|---|---|
| 4000–4099 | Not reconnect unchanged; something must change first (credential, configuration) |
| 4100–4199 | Reconnect after a backoff of at least 1 s, exponential with jitter |
| 4200–4299 | Reconnect immediately |

| v2 | v1 | Reason | Preceded by | When |
|---|---|---|---|---|
| 4001 | 1008 | `auth_expired` | `sys.kicked` | The credential expired without a refresh. Get a fresh one, then reconnect |
| 4003 | 1008 | `policy` | `sys.error` | Policy decided to close |
| 4004 | 1008 | `too_many_sessions` | `sys.error TOO_MANY_SESSIONS` | Per-user limit with `on_exceed: deny` |
| 4005 | 1008 | `replaced` | `sys.kicked` | Another connection used this `sid` |
| 4006 | 1008 | `max_sessions_exceeded` | `sys.kicked` | Evicted by a newer session (`kick_oldest`) |
| 4007 | 1008 | `disconnected` | `sys.kicked {reason}` | Your backend disconnected the user |
| 4009 | 1009 | `message too big` | | Frame over `max_frame_bytes` |
| 4010 | 1002 | `decode_error` | `sys.error BAD_REQUEST` | Malformed envelope or frame |
| 4100 | 1013 | `capacity` | `sys.error RESOURCE_EXHAUSTED` | Tenant capacity reached during admission |
| 4101 | 1008 | `slow_consumer` | `sys.error SLOW_CONSUMER` | Outbound queue full for a reliable message |
| 4102 | 1013 | `unavailable` | `sys.error UNAVAILABLE` | The cluster's session store is unavailable |
| 4103 | 1001 | `draining` | | The node is shutting down |
| 4200 | 1000 | `idle_timeout` | `sys.error TIMEOUT` | No inbound activity |

Treat a standard 1009 like 4009.

## Ping and idle

The gateway pings every `ping_interval_ms`. Any inbound frame, including a pong, counts as activity; a connection silent for `idle_timeout_ms` is closed with 4200.

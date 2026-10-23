# The Hot Lane

The Hot lane carries state that changes faster than anyone needs to see every intermediate value: positions, cursors, aim direction, a fader on a mixing desk, the best bid. It trades the Ext lane's guarantees for low latency and bounded queues.

## Frames

Hot frames are binary WebSocket messages. Under protocol v2 a client sends:

```
[kind=0x01][flags u8][chan u32 LE][svc u8][op u8][key u32 LE?][seq u32 LE?][payload…]
```

and receives the same layout with the publisher's `sender u32` inserted before the payload. The flags say which optional fields are present:

| Flag | Field | Meaning |
|---|---|---|
| `0x01` | `key` | The frame is state for this key; newer values replace older ones |
| `0x02` | `seq` | A sequence number, used by receivers to drop reordered frames |
| `0x04` | `sender` | Set by the server only; the publisher's `member_id` |

`chan` is the room id from `sys.joined`. A frame naming a room the session has not joined is dropped and counted (`policy_decisions{reason="unknown_channel"}`). With `hot_error_mode: sys_error` the client is also told; `silent` saves the bandwidth.

The payload is opaque to the gateway. Encode it however suits your application: a few floats, a FlatBuffer, a protobuf.

Protocol v1 clients use an older, simpler header (`[v][svc][op][flags][seq?][payload]`) and send to their *active room*, the last one they joined. Both kinds of client can share a room; v1 receivers get the payload only.

## Routing and permission

A Hot frame is handled by the service whose id is in `svc`. Service `1` is the built-in **room relay**, which forwards the frame to the other members of the room. Your own Hot services are registered in Rust with `Gateway::builder().hot_service(...)`.

The tenant's `policy.hot_allowlist` lists the `svc:op` pairs clients may send, for instance `"1:*"` for the relay with every opcode. An empty list (the default) disables the Hot lane for that tenant. Every listed service must exist, or startup fails.

## Keys and conflation

The key is what makes the Hot lane more than a lossy pipe. A frame with a key is *state*: when a recipient's queue is behind, the gateway keeps only the newest frame per `(sender, svc, key)` for that recipient and drops the ones it supersedes. A client that stalls for half a second receives the current positions of everyone in the room, not thirty stale positions each.

Use one key per independent piece of state: one per entity a player controls, say, or one per cursor. Frames without a key are events and are delivered individually; when a recipient's queue is full, they are dropped for that recipient.

The counters `wsprism_hot_slot_overwritten_total` (state replaced before it was sent; normal, high means readers lag) and `wsprism_egress_drop_total` show how much conflation and dropping is happening.

## Sequence numbers

Receivers should drop a frame whose `seq` is older than the last one they saw for the same sender and key. Over WebSocket frames arrive in order, but a transport with datagrams can reorder them. The JavaScript SDK does this for you (it handles `u32` wraparound) and numbers the frames it sends per key.

## Pacing with ticks

By default keyed state is sent as soon as it arrives. With `policy.hot_tick_hz` set (1 to 240), each session's pending keyed state is flushed at most that many times per second, at a phase chosen per session so that flushes do not all land at the same instant. When updates arrive faster than the tick, this saves both CPU and bandwidth, at the cost of up to one tick of extra latency. Unkeyed frames are never paced.

A 30 Hz tick is a reasonable starting point for cursors and casual games; 60 Hz suits action games.

## Echo to sender

`policy.hot_echo_to_sender` (default `true`) controls whether a session receives its own frames back through the relay. State-sync clients usually already know their own state, so `false` saves about 1/N of a room's Hot traffic.

## Publishing from the backend

The server API can publish Hot frames too:

```json
POST /api/v1/publish
{"room": "match:42", "hot": {"svc": 1, "op": 2, "key": 0, "data_b64": "AAEC"}}
```

They arrive from sender `0`. In a cluster they reach members on every node.

## What "lossy" means today

Over WebSocket, Hot frames are lossy *inside the gateway only*. Once a frame leaves the server it rides TCP, so a lost packet still delays every later frame until it is retransmitted (head-of-line blocking). On a clean network this costs nothing; on a lossy mobile link it shows up as stutter.

Truly unreliable delivery needs datagrams. We measured a WebTransport datagram path against WebSocket over TLS on an emulated 1% loss link at 60 Hz: the excess staleness of state fell three- to fourfold and the share of visibly stale frames fell from about 3% to almost zero, at roughly twice the CPU and memory per connection. WebTransport is therefore planned as an option for small interactive rooms, with WebSocket kept for large broadcast rooms. See [Benchmarks](../project/benchmarks.md) and [Roadmap](../project/roadmap.md).

## A worked example

Shared cursors in a document room, at 30 Hz, without echoing a user's own cursor back:

```yaml
tenants:
  - id: docs
    policy:
      ext_allowlist: ["room:*"]
      hot_allowlist: ["1:1"]
      hot_tick_hz: 30
      hot_echo_to_sender: false
      presence_events: true
```

```js
await client.join("doc:7f3a");
window.addEventListener("pointermove", (e) => {
  const buf = new Float32Array([e.clientX, e.clientY]);
  client.sendState("doc:7f3a", { svc: 1, op: 1, key: 0, payload: new Uint8Array(buf.buffer) });
});
client.on("state", (s) => drawCursor(s.sender, new Float32Array(s.payload.slice().buffer)));
client.on("presence", (p) => p.type === "left" && removeCursor(p.memberId));
```

Each user sends as often as the mouse moves; each viewer receives at most 30 updates per second per cursor, always the latest.

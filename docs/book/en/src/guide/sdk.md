# The JavaScript SDK

`sdk/js/wsprism.js` is the reference client: one ES module with no dependencies, for browsers and Node.js 22 or newer, with TypeScript declarations in `wsprism.d.ts`. It speaks protocol v2 and takes care of everything a hand-written client tends to get subtly wrong: reconnect policy, rejoining rooms, resuming history, deduplication, stale state, credential renewal.

## Connecting

```js
import { WsPrismClient } from "./wsprism.js";

const client = new WsPrismClient({
  url: "wss://rt.example.com/v1/ws",
  tenant: "acme",
  getTicket: () => fetch("/api/realtime-ticket").then((r) => r.text()),
});

await client.connect();
```

`getTicket` is called before every connection attempt and before every credential refresh. Return a one-time ticket from your backend or a fresh JWT. The credential travels in the `Sec-WebSocket-Protocol` header, never in the URL.

For several gateway nodes without a load balancer, pass a list: `url: ["wss://a/v1/ws", "wss://b/v1/ws"]`. Each reconnect tries the next one.

## Rooms and messages

```js
const { chan, memberId, members } = await client.join("match:42");   // members: with presence events on
client.send("chat", "send", { room: "match:42", data: { msg: "hi" } });
client.on("message", (m) => console.log(m.svc, m.type, m.data));
await client.leave("match:42");
```

`join`, `leave`, `request` and `rpc` return promises that resolve with the gateway's reply, matched by `seq`. Gateway and backend errors reject with a `WsPrismError` whose `code` is one of the [error codes](../reference/protocol.md#error-codes). `send` does not wait; its errors arrive as `error` events.

## Calling your backend

```js
const result = await client.rpc("game", "move", { room: "match:42", data: { x: 1 } });
```

This goes through the RPC proxy to your backend and resolves with the `data` it returned.

## State on the Hot lane

```js
client.sendState("match:42", { svc: 1, op: 1, key: 0, payload: new Uint8Array([1, 2]) });
client.on("state", (s) => render(s.room, s.sender, s.key, s.payload));
```

The SDK numbers outgoing frames per key and drops incoming frames older than the last one seen for the same `(room, sender, svc, key)`, with `u32` wraparound handled. `s.payload` is a view into the received buffer; copy it if you keep it.

## Reconnection

The SDK reads the close code and acts on its range:

| Close code | What the SDK does |
|---|---|
| 4000–4099 | Stops: the session was replaced, disconnected by your backend, refused by policy. Reconnecting unchanged would fail again |
| 4001 `auth_expired` | Reconnects at once with a fresh `getTicket()` |
| 4100–4199 | Reconnects after exponential backoff with jitter, 1 s to 30 s (capacity, slow consumer, cluster unavailable, draining) |
| 4200–4299 | Reconnects at once (idle timeout) |
| network failure, refused handshake, failing `getTicket()` | Backs off and retries |

An attempt that is not admitted within `connectTimeoutMs` (10 s by default) is abandoned and retried like any other failure. A stalled proxy or a half-open connection cannot leave the client stuck in `connecting`.

After a reconnect, every joined room is joined again and a `rejoined` event lists them. A room's `chan` and `memberId` may change. If a rejoin fails for a passing reason (`UNAVAILABLE`, `RATE_LIMITED`, `TIMEOUT`, `INTERNAL`), the SDK retries it on the same connection with backoff from 0.5 s to 30 s. If the connection drops during the rejoin, the next connection tries again. Only a refusal such as `NOT_ALLOWED` drops the room, with an `error` event explaining why.

## History and gaps

Rooms with history are resumed on rejoin from the last offset seen: missed messages arrive in order as ordinary `message` events, and duplicates are dropped by offset. Two signals mean the client must reload a room's state from your backend:

- a room in the `rejoined` list with `recovered: false`;
- a `gap` event, `{room, epoch?}`. With `epoch`, the room's history restarted, and the SDK resets its position for that room on its own.

## Presence

With `policy.presence_events` on, `join()` resolves with the current `members`, and `presence` events report `{type: "joined" | "left", room, user, memberId}`. `memberId` equals the `sender` of that member's state frames.

## Credential renewal

When the gateway reports an expiry (`expires_in_s`, for JWTs and connect-proxy credentials), the SDK renews at a random point between 70% and 90% of the lifetime: it calls `getTicket()` and sends `sys.refresh` on the open connection, then emits `refreshed`. Failed renewals are retried. If they keep failing, the gateway closes with 4001 at expiry and the SDK reconnects with a new credential. Pass `refresh: false` to turn renewal off.

## Events

| Event | Payload |
|---|---|
| `open` | the session was admitted: `{tenant, user, sid, expires_in_s?}` |
| `reconnecting` | `{attempt, delayMs}` before each new attempt |
| `message` | an Ext message: room broadcasts, server publishes |
| `state` | a Hot frame: `{room, chan, sender, svc, op, key, seq, payload}` |
| `presence` | `{type, room, user, memberId}` |
| `rejoined` | the rooms joined again after a reconnect |
| `gap` | `{room, epoch?}` |
| `refreshed` | the credential was renewed |
| `close` | `{code, reason, wasOpen, willReconnect}` |
| `error` | errors from `send`, failed rejoins, `TICKET_FAILED`, `CONNECT_TIMEOUT`, `REFRESH_FAILED` |

`client.state` is `idle`, `connecting`, `open` or `closed`, and `client.rooms` maps each joined room to `{room, chan, memberId, members, epoch?, offset?, recovered?}`.

## Testing

The SDK's tests run against a real gateway process:

```bash
sdk/js/run-tests.sh
npx -p typescript@5 tsc -p sdk/js    # checks the type declarations
```

## Other languages

The protocol is documented completely in [Wire Protocol](../reference/protocol.md), and any WebSocket library can speak it. SDKs for Unity (C#) and Dart are on the roadmap.

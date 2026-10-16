# wsPrism JavaScript client

Dependency-free ES module for browsers and Node 22+ (global `WebSocket`).
Supported Node versions: every non-EOL LTS line from 22 (CI tests 22 and
24; development uses `.nvmrc` = 24).
Speaks protocol v2 (`wsprism.v2.json`). Files: `wsprism.js` (the client), `wsprism.d.ts` (types).

```js
import { WsPrismClient } from "./wsprism.js";

const client = new WsPrismClient({
  url: "wss://rt.example.com/v1/ws",
  tenant: "acme",
  // Called before every connection attempt. Return a one-time ticket your
  // backend got from POST /api/v1/tickets, or a fresh JWT.
  getTicket: () => fetch("/api/realtime-ticket").then((r) => r.text()),
});

client.on("message", (m) => console.log(m.svc, m.type, m.data)); // room broadcasts, server publishes
client.on("state", (s) => render(s.room, s.sender, s.key, s.payload)); // Hot lane (stale seq dropped)
client.on("close", (c) => console.log("closed", c.code, c.willReconnect));

await client.connect();
await client.join("match:42");
client.send("chat", "send", { room: "match:42", data: { msg: "hi" } });
const result = await client.rpc("game", "move", { room: "match:42", data: { x: 1 } }); // tenants[].rpc
client.sendState("match:42", { svc: 1, op: 1, key: 0, payload: new Uint8Array([1, 2]) });
```

## Behaviour

- **Credentials** go in the `Sec-WebSocket-Protocol` header (`wsprism.auth.<ticket>`), never in the URL. Tickets must be HTTP token characters (one-time `wpt_` tickets and JWTs are).
- **Requests** (`request`, `join`, `leave`, `rpc`) get a `seq` and resolve with the matching `reply_to`. Gateway or backend errors reject with `WsPrismError` (`err.code`). `send` doesn't wait; its errors arrive as `error` events.
- **Reconnects** follow the close-code ranges: 40xx stop (a backend disconnect, a replaced session, policy...). The exception is `4001 auth_expired`, which reconnects at once with a fresh `getTicket()`. 41xx reconnect after exponential backoff with jitter (1 s to 30 s). 42xx reconnect at once. Network failures and rejected handshakes back off. Joined rooms are re-joined after a reconnect (`rejoined` event); room `chan` and `memberId` may change.
- **State** (Hot lane): frames older than the last `seq` seen for the same `(room, sender, key)` are dropped (u32 wraparound aware). `sendState` numbers frames per key.

There is no in-band token refresh: when a token expires the gateway closes the connection with 4001, and the client reconnects with a new token.

## Tests

```sh
sdk/js/run-tests.sh   # builds the gateway, starts it on 127.0.0.1, runs node:test
npx -p typescript@5 tsc -p sdk/js   # checks wsprism.d.ts against test/types-check.ts
```

The tests run against a real gateway process: one-time tickets come from the server API, an expiring JWT exercises the 4001 reconnect, and a local HTTP server acts as the RPC backend.

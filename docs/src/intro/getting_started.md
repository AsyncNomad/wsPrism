# Getting Started

Run the gateway, connect a client, and see where your backend plugs in.

## 1. A minimal config

```yaml
version: 1
gateway:
  listen: "0.0.0.0:8080"
  api: { listen: "127.0.0.1:8081" }      # internal: server API + /metrics
tenants:
  - id: acme
    auth:
      - kind: dev                         # development only: token "dev.<user>"
    policy:
      ext_allowlist: ["room:*", "chat:*"]
      hot_allowlist: ["1:*"]              # built-in Hot relay
      allowed_origins: ["http://localhost:3000"]
```

Every option is explained in the [reference config](https://github.com/AsyncNomad/wsPrism/blob/main/wsprism.yaml) and
[docs/CONFIG.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/CONFIG.md). For production, replace `dev` with
one-time tickets, JWT or the connect proxy.

## 2. Run and check

```bash
cargo install wsprism-gateway             # after the first release; from source: cargo run --release -p wsprism-gateway --
wsprism-gateway --config wsprism.yaml --check-config
wsprism-gateway --config wsprism.yaml

curl http://localhost:8080/healthz        # ok (public)
curl http://127.0.0.1:8081/metrics        # Prometheus (internal listener only)
```

## 3. Connect

With the [JavaScript SDK](https://github.com/AsyncNomad/wsPrism/blob/main/sdk/js/README.md) (browsers, Node 22+):

```js
import { WsPrismClient } from "./wsprism.js";

const client = new WsPrismClient({
  url: "ws://localhost:8080/v1/ws",
  tenant: "acme",
  getTicket: async () => "dev.alice",    // in production: a one-time ticket from your backend
});
client.on("message", (m) => console.log(m));
await client.connect();
await client.join("lobby");
client.send("chat", "send", { room: "lobby", data: { msg: "hello" } });
```

Without the SDK, offer the subprotocols `wsprism.v2.json` and
`wsprism.auth.<ticket>` (browsers), or send `Authorization: Bearer <ticket>`
(native clients). The wire format is in [docs/PROTOCOL.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/PROTOCOL.md).

## 4. Connect your backend

- **Who connects:** your backend issues one-time tickets (`POST /api/v1/tickets`),
  or the gateway verifies your JWTs, or asks your backend (connect proxy).
- **Push to clients:** `POST /api/v1/publish` to a room, `/api/v1/disconnect` a user.
- **Business logic in any language:** route `svc:type` requests to your backend
  over HTTP (RPC proxy); answers come back as `sys.reply`.
- **In Rust:** add your own services and authenticators with `Gateway::builder`
  ([docs/API.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/API.md)).

A complete example (Node backend + browser page): [examples/chat-and-state](https://github.com/AsyncNomad/wsPrism/blob/main/examples/chat-and-state/README.md).

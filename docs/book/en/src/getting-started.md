# Getting Started

This chapter runs a gateway on your machine, connects a client to it, and shows where your backend fits in. It takes about ten minutes. You need a Rust toolchain (1.95 or newer) and, for the client, Node.js 22 or newer.

## 1. Build the gateway

wsPrism has not been published to crates.io yet, so build it from the repository:

```bash
git clone https://github.com/AsyncNomad/wsPrism
cd wsPrism
cargo build --release -p wsprism-gateway
```

The binary is `target/release/wsprism-gateway`. A container image can be built from the `Dockerfile` at the repository root (see [Operations](guide/operations.md)).

## 2. Write a configuration

Save this as `quickstart.yaml`:

```yaml
version: 1
gateway:
  listen: "127.0.0.1:8080"
  api:
    listen: "127.0.0.1:8081"      # internal: server API, /metrics, probes
tenants:
  - id: demo
    auth:
      - kind: dev                 # development only: accepts "dev.<user>"
    policy:
      ext_allowlist: ["room:*", "chat:*"]
      hot_allowlist: ["1:*"]      # the built-in Hot relay
```

A few things are worth noticing even in a file this small:

- Everything belongs to a **tenant**. Clients name their tenant when they connect, and each tenant has its own authentication, policy and limits.
- The `dev` authenticator admits anyone who presents `dev.<name>`. It exists so you can try things without a backend. Never deploy it.
- The allowlists are the whole list of what clients may send. Without `chat:*`, the `chat.send` message used below would be refused with `NOT_ALLOWED`.

Check the file before running it. The parser is strict and points at the exact field when something is wrong:

```bash
./target/release/wsprism-gateway --config quickstart.yaml --check-config
```

## 3. Run it

```bash
RUST_LOG=info ./target/release/wsprism-gateway --config quickstart.yaml
```

In another terminal:

```bash
curl http://127.0.0.1:8080/healthz     # ok
curl http://127.0.0.1:8080/readyz      # ready
curl -s http://127.0.0.1:8081/metrics | head
```

Metrics are served only on the internal listener. They name your tenants and reveal load, so they do not appear on the public port unless you ask for it.

## 4. Connect a client

The JavaScript SDK is a single dependency-free file, `sdk/js/wsprism.js`. Save the following as `hello.mjs` in the repository root and run it with `node hello.mjs`:

```js
import { WsPrismClient } from "./sdk/js/wsprism.js";

const client = new WsPrismClient({
  url: "ws://127.0.0.1:8080/v1/ws",
  tenant: "demo",
  getTicket: async () => "dev.alice",
});

client.on("message", (m) => console.log("message:", m.svc, m.type, m.data));

await client.connect();
await client.join("lobby");
client.send("chat", "send", { room: "lobby", data: { msg: "hello" } });
```

Run it twice, in two terminals, with different names: each client sees the other's messages. `getTicket` is called before every connection attempt. In production it fetches a fresh credential from your backend, which is why it is a function and not a string.

Browsers work the same way, with one extra step: a page's origin must be listed in the tenant's `policy.allowed_origins`, otherwise the handshake is refused with 403. Node clients send no `Origin` header and are not checked.

## 5. Connect your backend

The dev authenticator stands in for the part your backend normally plays. A real deployment replaces it with one of these:

- **One-time tickets.** Your backend authenticates the user its usual way, asks the gateway for a short-lived ticket (`POST /api/v1/tickets`), and hands it to the client.
- **JWT.** If you already issue JWTs, the gateway can verify them itself, with static keys or your identity provider's JWKS endpoint.
- **Connect proxy.** The gateway forwards the token to an HTTP endpoint of yours and lets it decide.

From there, your backend publishes to rooms (`POST /api/v1/publish`), disconnects users (`POST /api/v1/disconnect`) and can answer client requests routed to it over HTTP. [Connecting Your Backend](guide/backend.md) walks through each of these.

## A complete example

`examples/chat-and-state` is a small application with a Node backend and a browser page: login with one-time tickets and room permissions, chat, a dice roll answered by the backend through the RPC proxy, and live cursors on the Hot lane with presence events. Start it with:

```bash
examples/chat-and-state/run.sh
```

and open <http://localhost:3000> in two browser windows.

## Next steps

- [Core Concepts](guide/concepts.md) defines tenants, sessions, rooms and lanes properly.
- [Configuration](reference/configuration.md) lists every setting with its default and range.
- [Running a Cluster](guide/cluster.md) adds a second node and Redis.

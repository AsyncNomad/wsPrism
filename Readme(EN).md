<div align="center">

<img src="docs/src/assets/logo.png" alt="wsPrism Logo" width="600px">

# wsPrism

**The Enterprise-Grade Realtime Gateway**

**Native Performance. Universal Extensibility.**

</div>

<p align="center">
  <a href="https://asyncnomad.github.io/wsPrism/">
    <img src="https://img.shields.io/badge/📖_Project-Documentation-blue?style=for-the-badge" alt="Project Documentation">
  </a>
  &nbsp;
  <a href="https://asyncnomad.github.io/wsPrism/api/wsprism/index.html">
    <img src="https://img.shields.io/badge/⚙️_Rust-API_Docs-orange?style=for-the-badge&logo=rust" alt="Rust API Docs">
  </a>
</p>

---

## Introduction

   **wsPrism** is a high-performance, standalone realtime gateway designed to sit between your clients (Frontend/Game Client) and your business logic (Backend).

   While modern web backends are excellent at handling RESTful requests, they often struggle with the heavy concurrency, statefulness, and extreme low-latency requirements of persistent socket connections. **wsPrism solves this by offloading the heavy lifting of connection management to a dedicated, Rust-powered process.**

   We built wsPrism with a specific philosophy: **"The core should be fast and safe (Rust), but the usage should be universal."** You do not need to be a Rust expert to use wsPrism. It is designed to be an enterprise-grade platform that is highly configurable, secure by default, and extensible to fit any domain—from multiplayer games and trading platforms to live collaborative tools.

---

## Quick Start

Get wsPrism up and running in seconds.

### 1. Configuration

wsPrism controls all gateway behavior through a single wsprism.yaml file located at the project root. By customizing this file, operators can apply completely different policies on a per-tenant basis. Through this configuration, the following aspects can be defined:

- Network behavior such as the WebSocket server address, ping interval, and idle timeout

- Isolated policies at the tenant (project) level

- Rate limiting strategies including requests per second and burst capacity

- Session policies that define concurrent connections per user and behavior on exceed (1:1 / 1:N)

- Error handling modes and usage conditions for the low-latency binary path (Hot Lane)

- Deny-by-default, allowlist-based routing that permits only explicitly allowed messages

With this approach, wsPrism can safely and consistently support diverse requirements—such as games, chat applications, collaborative tools, and financial systems—purely through configuration, without any code changes.

### 2. Run the Gateway

```bash
# Validate the configuration only (for CI / pre-deploy checks)
cargo run --release -p wsprism-gateway -- --check-config --config wsprism.yaml

# Run (config path via --config or WSPRISM_CONFIG; default ./wsprism.yaml)
RUST_LOG=wsprism_gateway=info cargo run --release -p wsprism-gateway -- --config wsprism.yaml

# Built binary
./target/release/wsprism-gateway --config wsprism.yaml

# Container (distroless, non-root; mount the config; JSON logs)
docker build -t wsprism-gateway .
docker run -v $PWD/my.yaml:/etc/wsprism/wsprism.yaml:ro -p 8080:8080 wsprism-gateway
# Two-node cluster on Redis: deploy/compose/
```

### 3. Verify

```
INFO wsprism_gateway: wsprism-gateway starting config=wsprism.yaml
INFO wsprism_gateway::gateway: wsprism-gateway serving listen=0.0.0.0:8080 api=127.0.0.1:8081
```

```bash
curl http://localhost:8080/healthz   # ok
curl http://localhost:8080/readyz    # ready (503 while draining, or when a cluster node lost its slot)
curl http://127.0.0.1:8081/metrics   # Prometheus metrics (internal listener gateway.api.listen; not on the public port)
```

### 4. Connect

- **URL:** `ws://<host>:<port>/v1/ws?tenant=<id>[&sid=<session id>]`
- **The ticket** is sent through one of the tenant's `policy.ticket_sources` (default: header, subprotocol). The query string (`?ticket=`) is off by default because proxies log URLs.
  - Native clients: `Authorization: Bearer <ticket>`
  - Browsers: subprotocols `wsprism.v2.json` + `wsprism.auth.<ticket>` (v1 clients: `wsprism.v1`)
- Browser origins must be listed in the tenant's `policy.allowed_origins`. In the example configuration only `starter_plan` allows one (`https://app.example.com`): add your page's origin (e.g. `http://localhost:3000`) to try it locally.

```js
// JS SDK (sdk/js). The example tenants have no `auth`, so the development stub applies:
// ticket "dev" (user dev) or "dev.<id>" (user <id>). Real deployments use one-time tickets, JWT or a backend proxy (docs/CONFIG.md).
import { WsPrismClient } from "./sdk/js/wsprism.js";
const client = new WsPrismClient({ url: "ws://localhost:8080/v1/ws", tenant: "starter_plan", getTicket: async () => "dev.alice" });
await client.connect();
await client.join("lobby");
```

### 5. Send Messages (Protocol)

wsPrism uses a **Dual Lane** protocol. The full contract is in [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

**A. Ext Lane (Text/JSON)**
Used for chat, room management, and complex logic. Send a `seq` and responses come back with `reply_to`.

```json
{"v":1, "svc":"room", "type":"join", "room":"match:1", "seq":1}
{"v":1, "svc":"chat", "type":"send", "room":"match:1", "seq":2, "data":{"msg":"Hello World"}}
```

**B. Hot Lane (Binary)**
Used for latency-critical data (movement, physics). Routed by service id and opcode, delivered to the session's active room (the last room joined).

```
Structure: [v=1 (u8)][svc_id (u8)][opcode (u8)][flags (u8)][seq (u32 LE, if flags & 0x01)][payload ...]
Example:   [0x01][0x01][0x01][0x00][...bytes...]
```

---

## Design Philosophy

### 1. The "Sidecar" Architecture

wsPrism is not just a library; it is a **Realtime Edge Gateway**.

- It handles the "connection storm" (handshakes, heartbeats, ping/pong).
- It manages thousands of concurrent stateful sessions.
- It routes filtered, valid messages to your services.
- **Result:** Your main backend logic remains stateless and scalable, while wsPrism guarantees low-latency delivery.

### 2. Rust Performance, Human Accessibility

We utilize **Rust** for the Core Engine to guarantee memory safety, zero-cost abstractions, and stable tail latencies. However, wsPrism is designed as a **Platform**:

- **Configurable Policies:** Rate limits, packet sizes, and allowlists are managed via simple YAML configurations.
- **Ops-Ready:** Built-in metrics (Prometheus) and structured logging for enterprise observability.

### 3. The "Dual Lane" Protocol

Most frameworks force you to choose between "Easy JSON" or "Fast Binary." wsPrism unifies them:

- **Ext Lane (Text/JSON):** For chat, lobby logic, and complex data. Easy to debug, highly flexible.
- **Hot Lane (Binary):** For movement, physics, and market ticks. Zero-copy decoding, minimal overhead, routed strictly by ID.

---

## Key Features

- **⚡ Ultra-Low Latency Core:** Built on `tokio` and `axum`; session loops never wait on their own queues. Numbers in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).
- **🏢 Multi-Tenancy:** Single instance support for multiple isolated tenants (projects) with independent policies.
- **🛡️ Policy Engine:** "Secure by Default." Define strict allowlists, rate limits, and payload sizes per tenant.
- **🧠 Deterministic QoS:** Prioritized message delivery. "Lossy" channels for game state, "Reliable" channels for chat.
- **🔌 Transport Agnostic:** The business logic is decoupled from the transport layer (WebSocket today, WebTransport under evaluation).
- **📜 History & Recovery:** Per-room message history (offset/epoch); reconnecting clients get what they missed, in order.
- **🌐 Cluster (experimental):** Nodes share rooms, sessions, presence and history through Redis. Node kills, rolling updates and Redis restarts are chaos-tested ([`docs/CLUSTER.md`](docs/CLUSTER.md)).
- **🔧 Operations:** JSON logs (session `trace_id`, secrets masked), `${VAR}` in config values, Prometheus metrics, lossless graceful drain, Docker image ([`docs/OPERATIONS.md`](docs/OPERATIONS.md)).

---

## Architecture Overview

wsPrism acts as the traffic controller for your realtime infrastructure.

```mermaid
graph TD
    Client_A[Client] -->|WebSocket| Transport
    Client_B[Client] -->|WebSocket| Transport

    subgraph Process [wsPrism Process]
        Transport[Transport Layer]
        Policy[Policy Engine]
        
        subgraph Core [Realtime Core]
            Router[Dispatcher]
            Registry[Session Registry]
            Presence[Room Presence]
        end
        
        Transport --> Policy --> Router
        
        Router -->|Hot Lane - Binary| Service_Echo[Native/Hot Service]
        Router -->|Ext Lane - Text| Service_Chat[Native Service · WASM planned]
    end
    
    Service_Chat -.->|RPC proxy over HTTP| Backend[Your Backend API]
    Backend -.->|Server API: publish, tickets, disconnect| Transport
```

---

## Documentation

| Document | Contents |
|---|---|
| [`docs/PROTOCOL.md`](docs/PROTOCOL.md) | Wire protocol contract: handshake & auth, both lanes, rooms, error codes, close codes |
| [`docs/CONFIG.md`](docs/CONFIG.md) | Configuration: authentication (tickets, JWT, proxy, plugins), server API, RPC proxy, cluster, environment values |
| [`docs/OPERATIONS.md`](docs/OPERATIONS.md) | Operations: command line, JSON logs, metrics catalogue, stopping & updating, containers |
| [`docs/CLUSTER.md`](docs/CLUSTER.md) | Cluster: what to deploy, failure behaviour, rolling updates, delivery semantics, capacity |
| [`docs/API.md`](docs/API.md) | Rust API: custom services and authenticators via `Gateway::builder`, API tiers and compatibility |
| [`sdk/js/`](sdk/js/README.md) | JavaScript client SDK (no dependencies, protocol v2) |
| [`examples/chat-and-state/`](examples/chat-and-state/README.md) | Example app: Node backend + SDK (chat, RPC, Hot state) |
| [`CHANGELOG.md`](CHANGELOG.md) | Changes (all crates share one version) |
| [`docs/RELEASING.md`](docs/RELEASING.md) | Release procedure |
| [`wsprism.yaml`](wsprism.yaml) | Annotated reference for every configuration option |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | Methodology and performance numbers |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Milestones toward production grade |
| [`docs/adr/`](docs/adr/README.md) | Architecture decision records |
| [`docs/HARDENING.md`](docs/HARDENING.md) | Security & correctness fix log |
| [`SECURITY.md`](SECURITY.md) | How to report a vulnerability (privately) |

---

## ⚠️ Disclaimer & Liability

**Please Read Carefully:**

**wsPrism is currently an experimental, personal prototype project.**

While it is designed with enterprise-grade architecture and performance in mind, it is **provided "as is"**, without warranty of any kind, express or implied, including but not limited to the warranties of merchantability, fitness for a particular purpose, and non-infringement.

By using this software, you acknowledge and agree that:

1. **Use at Your Own Risk:** You are solely responsible for determining the appropriateness of using or redistributing this software.
2. **No Liability:** In no event shall the authors or copyright holders be liable for any claim, damages, or other liability, whether in an action of contract, tort, or otherwise, arising from, out of, or in connection with the software or the use or other dealings in the software.
3. **Not Production Certified:** This software has not yet undergone formal security audits or large-scale production verification.

If you plan to use wsPrism in a production environment, we strongly recommend conducting your own thorough testing and security reviews.

---

## License

This project is licensed under the **Apache License 2.0**. See the [LICENSE](LICENSE) file for details.

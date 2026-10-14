# Getting Started

This guide will help you get the wsPrism gateway up and running in minutes.

---

## 1. Configuration

First, create a `wsprism.yaml` file in your project root.  
This file controls networking, security policies, and tenant isolation.

> **Note:**  
> This is a minimal configuration for quick testing.  
> For a complete list of options (rate limiting, session modes, etc.), please refer to the **Configuration Guide**.

```yaml
version: 1

gateway:
  listen: "0.0.0.0:8080"
  ping_interval_ms: 5000
  idle_timeout_ms: 10000

tenants:
  - id: acme
    limits:
      max_frame_bytes: 65536
    policy:
      # Allow basic room and chat commands
      ext_allowlist:
        - "room:join"
        - "room:leave"
        - "chat:*"

      # Allow all binary opcodes for service ID 1
      hot_allowlist:
        - "1:*"

      # Browser pages allowed to connect (requests without Origin are not checked)
      allowed_origins:
        - "http://localhost:3000"
```

---

## 2. Run the Gateway

```bash
# Validate the configuration only (for CI / pre-deploy checks)
cargo run --release -p wsprism-gateway -- --check-config --config wsprism.yaml

# Run (config path via --config or WSPRISM_CONFIG; default ./wsprism.yaml)
RUST_LOG=wsprism_gateway=info cargo run --release -p wsprism-gateway -- --config wsprism.yaml
```

---

## 3. Verify

```
INFO wsprism_gateway: wsprism-gateway starting listen=0.0.0.0:8080 config=wsprism.yaml
```

```bash
curl http://localhost:8080/healthz   # ok
curl http://localhost:8080/readyz    # ready (503 while draining)
```

---

## 4. Connect

```
ws://<host>:<port>/v1/ws?tenant=<id>[&sid=<session id>]
```

The ticket travels through one of the tenant's `policy.ticket_sources`
(default: header, subprotocol). The query string is off by default because
proxies log URLs.

| Client | How to send the ticket |
|---|---|
| Native (apps, engines, servers) | `Authorization: Bearer <ticket>` |
| Browser | subprotocols `wsprism.v1` and `wsprism.auth.<ticket>` |

```js
// Browser. Its origin must be listed in policy.allowed_origins.
const ws = new WebSocket("ws://localhost:8080/v1/ws?tenant=acme", ["wsprism.v1", "wsprism.auth.dev"]);
```

> **Note:** authentication is currently a development stub: only the ticket
> `dev` is accepted. Pluggable authenticators (JWT, backend callback) are on
> the roadmap (M2).

---

## 5. Send Messages (Hello World)

The full wire contract is in `docs/PROTOCOL.md` in the repository.

### A. Ext Lane (Text / JSON)

Join a room, then send to it. A request `seq` comes back as `reply_to`.

```json
{"v":1, "svc":"room", "type":"join", "room":"match:1", "seq":1}
{"v":1, "svc":"chat", "type":"send", "room":"match:1", "seq":2, "data":{"msg":"Hello World"}}
```

### B. Hot Lane (Binary)

Routed by service id and opcode to the session's active room (the last room
joined).

```
[v=1 (u8)][svc_id (u8)][opcode (u8)][flags (u8)][seq (u32 LE, if flags & 0x01)][payload ...]
```

**Hex example** (service 1 = echo, opcode 1, no seq):

```
01 01 01 00 DE AD BE EF
```

---

## What's Next?

- Learn how to configure **Rate Limits** and **Session Policies** in the Configuration Guide.
- Understand the **Design Philosophy** behind the Dual Lane architecture.
- Explore the internal mechanics in the Architecture documentation.

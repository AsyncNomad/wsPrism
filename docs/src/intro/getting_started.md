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
```

---

## 2. Run the Gateway

You can start the gateway using **Cargo** (when developing from source) or a **pre-built binary**.

### Using Cargo

```bash
# Enable debug logging to see detailed startup information
RUST_LOG=wsprism_gateway=debug cargo run --release -- --config wsprism.yaml
```

### Using Binary

```bash
./wsprism start --config wsprism.yaml
```

---

## 3. Verify

Check your terminal output.  
You should see logs indicating that the server is active and the tenant configuration is loaded:

```text
INFO  wsprism::server > 🚀 wsPrism Gateway active at 0.0.0.0:8080
INFO  wsprism::loader > Loaded configuration for tenant: "acme"
```

---

## 4. Connect

Now connect a WebSocket client (browser console, Postman, game engine, etc.).

**URL format**

```
ws://<host>:<port>/v1/ws?tenant=<id>&ticket=<auth_token>
```

**Example**

```
ws://localhost:8080/v1/ws?tenant=acme&ticket=dev
```

> **Tip:**  
> The `ticket` parameter is currently a placeholder for development.  
> In production, this would typically be a signed JWT or session token.

---

## 5. Send Messages (Hello World)

Once connected, wsPrism allows communication via two distinct **lanes**.

### A. Ext Lane (Text / JSON)

Use this for chat, logic, and room management.

```json
{
  "v": 1,
  "svc": "chat",
  "type": "send",
  "room": "lobby",
  "data": {
    "msg": "Hello wsPrism!"
  }
}
```

---

### B. Hot Lane (Binary)

Use this for high-frequency data such as player movement or market ticks.

**Binary format**
```
[svc_id (u8)] [opcode (u8)] [flags (u8)] [payload...]
```

**Example**
- Service ID: `1`
- OpCode: `1`
- Flags: `0`
- Payload: arbitrary bytes

**Hex representation**
```
01 01 00 DE AD BE EF
```

---

## What's Next?

- Learn how to configure **Rate Limits** and **Session Policies** in the Configuration Guide.
- Understand the **Design Philosophy** behind the Dual Lane architecture.
- Explore the internal mechanics in the Architecture documentation.

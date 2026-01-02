# Configuration Guide

wsPrism is configured via a single YAML file (`wsprism.yaml`) located in your project root.  
This file controls networking, security policies, rate limits, session behavior, and tenant isolation.

---

## Full Configuration Example

Below is a comprehensive example showing different tenant configurations:
- **Standard SaaS**
- **High-Security (Banking-style)**
- **High-Throughput (Gaming-style)**

```yaml
# wsprism.yaml
# -----------------------
# wsPrism Gateway Config
# -----------------------

version: 1  # (required) Config schema version. Currently only 1 is supported.

gateway:
  # WebSocket server bind address
  listen: "0.0.0.0:8080"

  # Server-side heartbeat interval (milliseconds)
  ping_interval_ms: 5000

  # Idle timeout (milliseconds).
  # Connection closes if no inbound activity.
  # Must be greater than ping_interval_ms.
  idle_timeout_ms: 10000

tenants:
  # -----------------------
  # Tenant A: "acme" (Standard)
  # -----------------------
  - id: acme
    limits:
      max_frame_bytes: 65536

    policy:
      rate_limit_rps: 1000
      rate_limit_burst: 2000
      rate_limit_scope: both

      sessions:
        mode: multi
        max_sessions_per_user: 4
        on_exceed: kick_oldest

      hot_error_mode: silent
      hot_requires_active_room: true

      ext_allowlist:
        - "room:join"
        - "room:leave"
        - "chat:*"
      
      hot_allowlist:
        - "1:*"

  # -----------------------
  # Tenant B: "bank" (High Security)
  # -----------------------
  - id: bank
    limits:
      max_frame_bytes: 16384

    policy:
      rate_limit_rps: 100
      rate_limit_burst: 200
      rate_limit_scope: tenant

      sessions:
        mode: single
        max_sessions_per_user: 1
        on_exceed: deny

      hot_error_mode: sys_error
      hot_requires_active_room: true

      ext_allowlist:
        - "room:join"
        - "room:leave"
        - "chat:send"
      
      hot_allowlist: []

  # -----------------------
  # Tenant C: "game" (High Throughput)
  # -----------------------
  - id: game
    limits:
      max_frame_bytes: 65536

    policy:
      rate_limit_rps: 5000
      rate_limit_burst: 10000
      rate_limit_scope: connection

      sessions:
        mode: multi
        max_sessions_per_user: 10
        on_exceed: kick_oldest

      hot_error_mode: silent
      hot_requires_active_room: true

      ext_allowlist:
        - "room:*"
        - "chat:*"
        - "match:*"
      
      hot_allowlist:
        - "1:*"
        - "2:*"
```

---

## Schema Reference

### Root Fields

| Field     | Type     | Required | Description |
|----------|----------|----------|-------------|
| version  | integer  | Yes      | Config schema version. Currently only `1` is supported. |
| gateway  | object   | No       | Global network settings. |
| tenants  | array    | Yes      | List of isolated tenant configurations. |

---

## Gateway Section

| Field | Type | Default | Description |
|------|------|---------|-------------|
| listen | string | — | Bind address (e.g. `0.0.0.0:8080`). |
| ping_interval_ms | integer | 5000 | Interval for server-side PING frames. |
| idle_timeout_ms | integer | 10000 | Close connection if no inbound activity. Must be greater than `ping_interval_ms`. |

---

## Tenant Configuration

Each item in the `tenants` list represents an isolated project.

| Field | Type | Description |
|------|------|-------------|
| id | string | **Required.** Unique identifier. Used in connection URL: `?tenant=<id>`. |
| limits | object | Hard limits for resource protection. |
| policy | object | Logic for rate limiting, sessions, and routing. |

---

## Limits Object

| Field | Type | Description |
|------|------|-------------|
| max_frame_bytes | integer | Maximum allowed size for a single WebSocket frame. Prevents memory exhaustion attacks. |

---

## Policy Object

### 1. Rate Limiting

Token Bucket–based flow control.

| Field | Type | Description |
|------|------|-------------|
| rate_limit_rps | integer | Refill rate (requests per second). Must be > 0. |
| rate_limit_burst | integer | Burst capacity. Must be > 0. |
| rate_limit_scope | enum | `tenant`, `connection`, or `both`. |

**Scope behavior**
- `tenant`: One shared bucket for the entire tenant
- `connection`: Independent bucket per socket
- `both`: Enforces both limits

---

### 2. Session Management

Controls concurrency and multi-device behavior.

| Field | Type | Description |
|------|------|-------------|
| sessions.mode | enum | `single` (1 user = 1 session) or `multi` (1 user = N sessions). |
| max_sessions_per_user | integer | Maximum concurrent sessions. Must be `1` if mode is `single`. |
| on_exceed | enum | `deny` or `kick_oldest`. |

---

### 3. Hot Lane Behavior

Controls the binary (low-latency) data path.

| Field | Type | Description |
|------|------|-------------|
| hot_error_mode | enum | `sys_error` (send error frame) or `silent` (drop silently). |
| hot_requires_active_room | bool | If true, binary messages are rejected unless the user joined a room. |

---

### 4. Allowlists (Routing)

wsPrism operates on a **deny-by-default** model.  
All messages must be explicitly allowed.

| Field | Format | Examples |
|------|--------|----------|
| ext_allowlist | `<service>:<type>` | `room:join`, `chat:*` |
| hot_allowlist | `<service_id>:<opcode>` | `1:1`, `1:*` |

Wildcards (`*`) allow all types or opcodes for a service.

---

## Best Practices

### 🎮 Games / Chat / Collaboration
- Session Mode: `multi`
- Max Sessions: `3 ~ 10`
- On Exceed: `kick_oldest`
- Rate Limit Scope: `connection`
- Hot Error Mode: `silent`

**Goal:** Throughput + UX

---

### 🏦 Banking / High Security
- Session Mode: `single`
- Max Sessions: `1`
- On Exceed: `deny`
- Rate Limit Scope: `tenant`
- Avoid wildcards in allowlists

**Goal:** Security + strict control

# Configuration Guide

wsPrism is configured via a single YAML file (`wsprism.yaml`) located in your project root.  
This file controls networking, security policies, rate limits, session behavior, tenant isolation, and observability.

---

## Full Configuration Example

Below is a comprehensive example showing different tenant configurations:

- **Standard SaaS** – Balanced settings for general applications  
- **High-Security (Banking-style)** – Strict single-session enforcement and audit-friendly behavior  
- **High-Throughput (Gaming-style)** – Optimized for massive concurrency and low latency

```yaml
# wsprism.yaml
# ----------------=========================================----------------
# wsPrism Gateway Configuration
# ----------------=========================================----------------

version: 1  # (required) Config schema version. Currently only 1 is supported.

gateway:
  # -----------------------------------------------------------------------
  # 1. Network & Lifecycle
  # -----------------------------------------------------------------------
  listen: "0.0.0.0:8080"
  ping_interval_ms: 20000
  idle_timeout_ms: 60000
  writer_send_timeout_ms: 1500
  drain_grace_ms: 5000

  # -----------------------------------------------------------------------
  # 2. Handshake Defender (DoS Protection)
  # -----------------------------------------------------------------------
  handshake_limit:
    enabled: true
    global_rps: 500
    global_burst: 1000
    per_ip_rps: 10
    per_ip_burst: 20
    max_ip_entries: 50000

  # -----------------------------------------------------------------------
tenants:
  # =======================================================================
  # Tenant A: "acme" (Standard SaaS)
  # =======================================================================
  - id: acme
    limits:
      max_frame_bytes: 65536
      max_sessions_total: 10000
      max_rooms_total: 500
      max_users_per_room: 100
      max_rooms_per_user: 10

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

  # =======================================================================
  # Tenant B: "bank" (High Security)
  # =======================================================================
  - id: bank
    limits:
      max_frame_bytes: 16384
      max_sessions_total: 1000
      max_rooms_total: 0
      max_users_per_room: 0
      max_rooms_per_user: 0

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
        - "auth:verify"
        - "msg:secure"

      hot_allowlist: []

  # =======================================================================
  # Tenant C: "game" (High Throughput)
  # =======================================================================
  - id: game
    limits:
      max_frame_bytes: 65536
      max_sessions_total: 100000
      max_rooms_total: 5000
      max_users_per_room: 200
      max_rooms_per_user: 0

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
        - "match:*"

      hot_allowlist:
        - "1:*"
        - "2:10"
```

---

## Schema Reference

### Root Fields

| Field | Type | Required | Description |
|------|------|----------|-------------|
| version | integer | Yes | Config schema version. Currently only `1` is supported. |
| gateway | object | No | Global network, security, and observability settings. |
| tenants | array | Yes | List of isolated tenant configurations. |

---

## Gateway Section

### Network & Lifecycle

| Field | Type | Default | Description |
|------|------|---------|-------------|
| listen | string | — | Bind address (e.g. `0.0.0.0:8080`). |
| ping_interval_ms | integer | 5000 | Interval for server-side PING frames. |
| idle_timeout_ms | integer | 10000 | Close connection if no inbound activity. |
| writer_send_timeout_ms | integer | 1500 | Drop slow consumers. |
| drain_grace_ms | integer | 5000 | Graceful shutdown wait time. |

### Handshake Defender (DoS Protection)

| Field | Type | Default | Description |
|------|------|---------|-------------|
| enabled | bool | false | Enable pre-upgrade rate limiting. |
| global_rps | integer | 200 | Global handshake RPS limit. |
| global_burst | integer | 200 | Global burst capacity. |
| per_ip_rps | integer | 10 | Per-IP handshake RPS. |
| per_ip_burst | integer | 50 | Per-IP burst capacity. |
| max_ip_entries | integer | 50000 | Max IPs tracked in memory. |

### Observability

| Field | Type | Description |
|------|------|-------------|
| metrics.enabled | bool | Enable Prometheus metrics. |
| metrics.path | string | Metrics endpoint path. |

---

## Tenant Limits (Resource Governance)

Hard limits to prevent resource exhaustion.  
`0` means unlimited.

| Field | Type | Description |
|------|------|-------------|
| max_frame_bytes | integer | Max WebSocket frame size. |
| max_sessions_total | integer | Max concurrent sessions per tenant. |
| max_rooms_total | integer | Max active rooms. |
| max_users_per_room | integer | Max users per room. |
| max_rooms_per_user | integer | Max rooms a user may join. |

---

## Policy Object

### 1. Rate Limiting

Token Bucket–based flow control.

| Field | Type | Description |
|------|------|-------------|
| rate_limit_rps | integer | Refill rate (requests/sec). |
| rate_limit_burst | integer | Burst capacity. |
| rate_limit_scope | enum | `tenant`, `connection`, or `both`. |

---

### 2. Session Management

| Field | Type | Description |
|------|------|-------------|
| sessions.mode | enum | `single` or `multi`. |
| max_sessions_per_user | integer | Max concurrent sessions per user. |
| on_exceed | enum | `deny` or `kick_oldest`. |

---

### 3. Hot Lane (Binary Protocol)

| Field | Type | Description |
|------|------|-------------|
| hot_error_mode | enum | `sys_error` or `silent`. |
| hot_requires_active_room | bool | Require room join before binary messages. |

---

### 4. Allowlists (Routing Security)

Deny-by-default routing.

| Field | Format | Examples |
|------|--------|----------|
| ext_allowlist | `<service>:<type>` | `room:join`, `chat:*` |
| hot_allowlist | `<service_id>:<opcode>` | `1:*`, `2:10` |

---

## Best Practices

### 🎮 Games / Realtime Systems
- Session Mode: `multi`
- Rate Limit Scope: `connection`
- Hot Error Mode: `silent`

### 🏦 Banking / High Security
- Session Mode: `single`
- Rate Limit Scope: `tenant`
- Avoid wildcards in allowlists

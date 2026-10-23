# Server API

The server API is how your backend acts on the gateway. It is served on the internal listener (`gateway.api.listen`), which must not be reachable from the internet.

## Authentication

Every `/api/v1/*` request carries a tenant API key:

```
Authorization: Bearer <key>
```

The key identifies the tenant. Keys are configured per tenant as SHA-256 hashes:

```yaml
tenants:
  - id: acme
    api_keys:
      - name: backend
        sha256: "<printf %s \"$KEY\" | sha256sum>"
```

An unknown key gets 401. The same listener serves `/metrics`, `/healthz` and `/readyz` without a key.

## `POST /api/v1/tickets`

Issue a one-time ticket for a user. The tenant's `auth` must include `kind: ticket`.

```json
{
  "user": "alice",
  "ttl_s": 30,
  "permissions": { "subscribe": ["match:*"], "publish": ["match:42"] }
}
```

| Field | Default | Meaning |
|---|---|---|
| `user` | required | User id, 1–64 of `[A-Za-z0-9_-]` |
| `ttl_s` | 30 | Lifetime, 1–300 seconds |
| `permissions.subscribe` | unrestricted | Rooms the user may join; `[]` = none |
| `permissions.publish` | unrestricted | Rooms the user may publish to; `[]` = none |

Response: `200 {"ticket": "wpt_…", "expires_in_s": 30}`.

Errors: 400 if the tenant does not accept tickets or the body is invalid; 503 if too many tickets are outstanding (one million per node), or, with `Retry-After`, if the cluster's ticket store is unavailable.

## `POST /api/v1/publish`

Publish to a room.

Ext (JSON) message, reliable by default:

```json
{ "room": "match:42", "data": { "event": "goal" } }
```

Hot (binary) frame, lossy by default:

```json
{ "room": "match:42", "hot": { "svc": 1, "op": 2, "key": 0, "data_b64": "AAEC" } }
```

Optional `"qos": "reliable" | "lossy"` overrides the default.

Response: `200 {"recipients": n}`, where `n` counts the members on the node that took the request. In a cluster, members on other nodes receive the message too and are not counted.

Errors: 400 for an invalid room or body; 503 with `Retry-After` while the node drains or when the cluster's queue to Redis is full. Retry on another node.

A 200 means the message is queued for delivery, not that it was delivered.

## `POST /api/v1/disconnect`

Close a user's sessions.

```json
{ "user": "alice", "sid": "tab-1", "reason": "account suspended" }
```

`sid` and `reason` are optional. Without `sid`, every session of the user closes. `reason` is at most 123 bytes because it travels in the WebSocket close frame. Clients receive `sys.kicked` and close code 4007.

Response: `200 {"closed": n}`. 503 if the cluster's session store is unavailable.

## `GET /api/v1/rooms/{room}/presence`

```json
{ "room": "match:42", "users": ["alice", "bob"] }
```

Each user appears once, even with several sessions. In a cluster the list covers every node. 503 if the cluster's presence store is unavailable.

## Operational endpoints

| Endpoint | Meaning |
|---|---|
| `GET /healthz` | 200 while the process runs |
| `GET /readyz` | 200 when serving; 503 while draining or after a cluster node lost its slot |
| `GET /metrics` | Prometheus metrics |

## Calls the gateway makes to your backend

For completeness, the two request shapes the gateway sends you. Both carry `Authorization: Bearer <secret>` when `secret_env` is set.

**Connect proxy** (`auth: kind: proxy`):

```json
POST <url>
{ "tenant": "acme", "token": "<ticket>", "client_ip": "203.0.113.7" }
```

Admit with `200 {"user": "...", "permissions": {...}?, "expires_at": <unix seconds>?}`, reject with 401 or 403. Anything else is treated as unavailable (503 to the client).

**RPC proxy** (`rpc`):

```json
POST <url>
{ "tenant": "acme", "user": "alice", "sid": "tab-1", "svc": "game", "type": "move",
  "room": "match:42", "joined": true, "data": { } }
```

Answer with `200 {"data": ...}` or `200 {"error": {"code": "...", "msg": "..."}}`.

These shapes are part of the stable contract and only grow.

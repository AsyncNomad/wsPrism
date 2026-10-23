# 服务端 API

服务端 API 是后端操作网关的入口。它在内部监听地址（`gateway.api.listen`）上提供服务，绝不能让互联网访问到。

## 认证

每个 `/api/v1/*` 请求都要携带租户 API 密钥：

```
Authorization: Bearer <key>
```

密钥决定了租户。密钥按租户配置，保存的是 SHA-256 哈希：

```yaml
tenants:
  - id: acme
    api_keys:
      - name: backend
        sha256: "<printf %s \"$KEY\" | sha256sum>"
```

未知密钥返回 401。同一个监听地址上的 `/metrics`、`/healthz` 和 `/readyz` 无需密钥。

## `POST /api/v1/tickets`

为用户签发一次性票据。租户的 `auth` 中必须包含 `kind: ticket`。

```json
{
  "user": "alice",
  "ttl_s": 30,
  "permissions": { "subscribe": ["match:*"], "publish": ["match:42"] }
}
```

| 字段 | 默认值 | 含义 |
|---|---|---|
| `user` | 必填 | 用户 ID，1 到 64 个 `[A-Za-z0-9_-]` 字符 |
| `ttl_s` | 30 | 有效期，1~300 秒 |
| `permissions.subscribe` | 不限 | 可加入的房间；`[]` 表示一个都不行 |
| `permissions.publish` | 不限 | 可发布消息的房间；`[]` 表示一个都不行 |

响应：`200 {"ticket": "wpt_…", "expires_in_s": 30}`。

错误：租户不接受票据或请求体不合法时返回 400；未使用的票据过多（每个节点一百万张）时返回 503；集群的票据存储不可用时返回 503 并带 `Retry-After`。

## `POST /api/v1/publish`

向房间发布消息。

Ext（JSON）消息，默认可靠投递：

```json
{ "room": "match:42", "data": { "event": "goal" } }
```

Hot（二进制）帧，默认可丢弃：

```json
{ "room": "match:42", "hot": { "svc": 1, "op": 2, "key": 0, "data_b64": "AAEC" } }
```

可选的 `"qos": "reliable" | "lossy"` 用来覆盖默认值。

响应：`200 {"recipients": n}`，`n` 是接收该请求的节点上的成员数。在集群中，其他节点上的成员同样会收到消息，但不计入其中。

错误：房间名或请求体不合法时返回 400；节点正在排空或集群通往 Redis 的队列已满时，返回 503 并带 `Retry-After`，请换一个节点重试。

200 表示消息已进入投递队列，并不表示已经送达。

## `POST /api/v1/disconnect`

关闭某个用户的会话。

```json
{ "user": "alice", "sid": "tab-1", "reason": "account suspended" }
```

`sid` 和 `reason` 可选。不带 `sid` 时关闭该用户的全部会话。`reason` 会放进 WebSocket 关闭帧，因此最长 123 字节。客户端会收到 `sys.kicked` 和关闭码 4007。

响应：`200 {"closed": n}`。集群的会话存储不可用时返回 503。

## `GET /api/v1/rooms/{room}/presence`

```json
{ "room": "match:42", "users": ["alice", "bob"] }
```

即使一个用户有多个会话，也只出现一次。在集群中，列表涵盖所有节点。集群的在线状态存储不可用时返回 503。

## 运维接口

| 接口 | 含义 |
|---|---|
| `GET /healthz` | 进程在运行时返回 200 |
| `GET /readyz` | 正常服务时返回 200；排空期间或集群节点失去槽位后返回 503 |
| `GET /metrics` | Prometheus 指标 |

## 网关发往后端的请求

为完整起见，这里列出网关发给你的两种请求格式。配置了 `secret_env` 时，两者都会带上 `Authorization: Bearer <secret>`。

**连接代理**（`auth: kind: proxy`）：

```json
POST <url>
{ "tenant": "acme", "token": "<ticket>", "client_ip": "203.0.113.7" }
```

放行时返回 `200 {"user": "...", "permissions": {...}?, "expires_at": <Unix 秒>?}`，拒绝时返回 401 或 403。其他任何响应都视为不可用（客户端收到 503）。

**RPC 代理**（`rpc`）：

```json
POST <url>
{ "tenant": "acme", "user": "alice", "sid": "tab-1", "svc": "game", "type": "move",
  "room": "match:42", "joined": true, "data": { } }
```

以 `200 {"data": ...}` 或 `200 {"error": {"code": "...", "msg": "..."}}` 作答。

这些格式同样属于稳定契约，只增不改。

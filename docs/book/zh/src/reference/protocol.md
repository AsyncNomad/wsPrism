# 线路协议

这是客户端与网关之间的契约。用 JavaScript SDK 的话基本不需要看；要为其他平台写客户端，这一章就是你需要的全部内容。

## 版本

通过 WebSocket 子协议协商两种版本之一：

- **v2**（`wsprism.v2.json`）：当前版本，也是 SDK 的默认选择。Hot 帧显式指明所属房间，关闭码会告诉客户端该如何重连。
- **v1**（`wsprism.v1`）：为旧客户端保留。Hot 帧发往会话的*活动房间*，即最后加入的那个房间。

服务端按 v2、v1 的顺序选出自己支持的第一个版本，并且只回显这一个。客户端提供了子协议却没有任何受支持的版本时，返回 400；完全不提供子协议的客户端按 v1 处理。

### 兼容性

从 0.1.0 起，协议只增不改：可能新增字段、消息类型和错误码，但已有的名称、类型和含义保持不变。**客户端必须忽略自己不认识的字段。**

## 连接

```
ws(s)://<host>/v1/ws?tenant=<tenant>[&sid=<会话 ID>]
```

`sid` 可选，为 1 到 64 个 `[A-Za-z0-9_-]` 字符。用相同 `sid` 发起的新连接会顶替旧连接，旧连接以 `replaced` 关闭。

凭证放在租户 `ticket_sources` 允许的位置：

| 位置 | 请求 |
|---|---|
| `header` | `Authorization: Bearer <ticket>` |
| `subprotocol` | `Sec-WebSocket-Protocol: wsprism.v2.json, wsprism.auth.<ticket>` |
| `query` | `?ticket=<ticket>`（默认关闭） |

子协议令牌只能由 HTTP token 字符构成；不带填充的 base64url 和 JWT 都符合要求。

成功后，第一条消息是：

```json
{"v":1,"svc":"sys","type":"authed","data":{"tenant":"acme","user":"alice","sid":"tab-1"},"trace_id":"5f0c…"}
```

凭证有过期时间时，`data` 中还会带上 `expires_in_s`。

### 升级前的拒绝

| HTTP | 原因 |
|---|---|
| 400 | 租户不存在、`sid` 不合法，或没有提供受支持的协议版本 |
| 401 | 缺少凭证或凭证被拒 |
| 403 | 浏览器 `Origin` 不在 `allowed_origins` 中 |
| 429 + `Retry-After` | 握手限流 |
| 503 | 节点正在排空，请连接其他节点（不带 `Retry-After`） |
| 503 + `Retry-After` | 租户容量已满、认证后端不可用，或集群繁忙 |

## Ext 通道：JSON 信封

```json
{
  "v": 1,
  "svc": "chat",
  "type": "send",
  "seq": 123,
  "room": "party:1",
  "data": { }
}
```

| 字段 | 含义 |
|---|---|
| `v` | 信封格式版本，固定为 1；不是协议版本 |
| `svc`、`type` | 服务与消息类型，白名单按二者的组合检查 |
| `seq` | 可选的请求编号。针对该请求的每条响应都会带上 `"reply_to": <seq>` |
| `room` | 消息涉及的目标房间 |
| `data` | 负载，原样交给服务 |
| `flags` | 可选位域：`0x01` 含 seq，`0x02` 含 room，`0x04` 请求确认 |

处理某个请求时产生的响应（`sys.joined`、`sys.reply`、`sys.error`，乃至策略触发的关闭通知）都带有 `reply_to`。客户端没有请求过的消息，比如房间广播、在线状态事件、踢下线通知，一律不带。

### 字段约定

- 客户端自身请求的结果放在顶层（`room`、`chan`、`member_id`、`members`、`epoch`、`offset`、`recovered`）；负载以及关于他人的事件放在 `data` 中。
- `epoch`、`trace_id`、`chan` 和 `member_id` 都是不透明值，只用于比较和回传，不要解析。
- `offset` 是能放进 JavaScript 数字的整数。

### 内置消息

| 客户端发送 | 网关回复 |
|---|---|
| `room.join {room, data?: {recover: {epoch, offset}}}` | `sys.joined {room, chan, member_id, members?, epoch?, offset?, recovered?}` |
| `room.leave {room?}` | `sys.left {room}`；不带 `room` 时离开活动房间 |
| `chat.send {room, data}` | 广播到房间；发送者必须已加入该房间 |
| `sys.refresh {data: {token}}` | `sys.refreshed {data: {expires_in_s}}` |
| 路由到后端的请求 | `sys.reply {data}` |

| 网关主动发送 | 含义 |
|---|---|
| `server.publish {room, data}` | 由你的后端发布 |
| `sys.member_joined` / `sys.member_left {room, data: {user, member_id}}` | 在线状态事件（开启时） |
| `sys.gap {room, epoch?}` | 该房间可能丢了消息；带 `epoch` 表示其历史已重新开始 |
| `sys.kicked {data: {reason}}` | 服务端即将关闭该会话 |
| `sys.error {data: {code, msg}}` | 错误；如果是对某个请求的回复，会带 `reply_to` |

在带历史的房间里，每条房间消息都带有 `offset`；原本没有 `room` 的消息会补上 `room`。

## Hot 通道：二进制帧

所有整数均为小端序。

**v2，客户端发往服务端：**

```
[kind=0x01][flags u8][chan u32][svc u8][op u8][key u32]?[seq u32]?[payload…]
```

**v2，服务端发往客户端：**

```
[kind=0x01][flags u8][chan u32][svc u8][op u8][key u32]?[seq u32]?[sender u32][payload…]
```

| 标志 | 字段 |
|---|---|
| `0x01` | 含 `key` |
| `0x02` | 含 `seq` |
| `0x04` | 含 `sender`（仅限服务端；客户端设置该位或任何未知位都会导致解码错误） |

- `chan` 是 `sys.joined` 中的房间 ID，在同一个节点上对房间的所有成员都相同。
- `sender` 是发布者的 `member_id`；通过服务端 API 发布的帧，发送方为 `0`。
- 带 `key` 的帧会按接收方合并：落后的接收方对每个 `(sender, svc, key)` 只收到最新的一帧。
- 接收方应丢弃 `seq` 比同一发送方、同一键最后见到的序号更旧的帧。

**v1：**

```
[v=1][svc u8][op u8][flags u8][seq u32]?[payload…]      flags：0x01 含 seq，0x02 请求确认
```

v1 帧发往活动房间，v1 接收方只收到负载。

## 错误码

`sys.error` 的 `data.code` 取自以下固定集合，`data.msg` 是给人看的说明。请根据 `code` 分支处理，不要依赖 `msg`。内部错误不会透露具体原因。

| 错误码 | 含义 |
|---|---|
| `BAD_REQUEST` | 请求格式错误或不合法 |
| `AUTH_FAILED` | 认证失败 |
| `RATE_LIMITED` | 触发限流 |
| `PAYLOAD_TOO_LARGE` | 负载过大 |
| `NOT_ALLOWED` | 不被允许：不在白名单内、超出用户权限，或尚未加入该房间 |
| `RESOURCE_EXHAUSTED` | 达到租户、房间或用户限额 |
| `UNSUPPORTED_VERSION` | 不支持的协议版本 |
| `TOO_MANY_SESSIONS` | 达到每用户会话上限 |
| `SLOW_CONSUMER` | 客户端跟不上 |
| `TIMEOUT` | 空闲超时内没有任何活动 |
| `UNAVAILABLE` | 依赖的服务不可用或已饱和（你的后端、集群的 Redis），请稍后重试 |
| `INTERNAL` | 内部错误 |

经由 RPC 代理由后端处理的请求，还可能带有后端自定义的错误码（`[A-Z0-9_]{1,32}`）。

## 关闭码

如果有需要说明的 `sys.*` 消息，网关会先发送它，然后关闭连接。在 v2 中，关闭码所在的区间告诉客户端接下来该怎么做：

| 区间 | 客户端应当 |
|---|---|
| 4000~4099 | 不要原样重连；需要先换凭证或修改配置 |
| 4100~4199 | 至少等待 1 秒，采用带抖动的指数退避后再重连 |
| 4200~4299 | 立即重连 |

| v2 | v1 | 原因 | 之前发送 | 何时 |
|---|---|---|---|---|
| 4001 | 1008 | `auth_expired` | `sys.kicked` | 凭证过期且未续期。取得新凭证后再重连 |
| 4003 | 1008 | `policy` | `sys.error` | 策略决定关闭 |
| 4004 | 1008 | `too_many_sessions` | `sys.error TOO_MANY_SESSIONS` | `on_exceed: deny` 下达到每用户上限 |
| 4005 | 1008 | `replaced` | `sys.kicked` | 另一条连接使用了这个 `sid` |
| 4006 | 1008 | `max_sessions_exceeded` | `sys.kicked` | 被更新的会话挤下线（`kick_oldest`） |
| 4007 | 1008 | `disconnected` | `sys.kicked {reason}` | 后端将该用户踢下线 |
| 4009 | 1009 | `message too big` | | 帧超过 `max_frame_bytes` |
| 4010 | 1002 | `decode_error` | `sys.error BAD_REQUEST` | 信封或帧格式错误 |
| 4100 | 1013 | `capacity` | `sys.error RESOURCE_EXHAUSTED` | 准入时租户容量已满 |
| 4101 | 1008 | `slow_consumer` | `sys.error SLOW_CONSUMER` | 可靠消息放不进发送队列 |
| 4102 | 1013 | `unavailable` | `sys.error UNAVAILABLE` | 集群的会话存储不可用 |
| 4103 | 1001 | `draining` | | 节点正在关闭 |
| 4200 | 1000 | `idle_timeout` | `sys.error TIMEOUT` | 没有入站活动 |

标准关闭码 1009 请按 4009 处理。

## 心跳与空闲

网关每隔 `ping_interval_ms` 发送一次 ping。任何入站帧（包括 pong）都算作活动；在 `idle_timeout_ms` 内毫无动静的连接以 4200 关闭。

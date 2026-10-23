# 指标

内部监听地址上的 `GET /metrics` 返回 Prometheus 文本格式。标签值只来自配置或固定集合，从不取自客户端输入，因此时间序列的数量由租户数量决定上限。

## 连接

| 指标 | 类型 | 标签 | 含义 |
|---|---|---|---|
| `wsprism_ws_upgrades_total` | counter | `tenant`、`status` | 已接受的 WebSocket 升级 |
| `wsprism_ws_sessions_active` | gauge | `tenant` | 当前打开的会话 |
| `wsprism_handshake_rejections_total` | counter | `tenant`、`reason` | 在升级前被拒绝的连接 |
| `wsprism_draining` | gauge | | 节点为关闭而排空期间为 1 |

请求指向未配置的租户时，`tenant` 为 `_unknown`。`reason` 取以下值之一：

| 原因 | HTTP | 说明 |
|---|---|---|
| `rate_limit` | 429 | 握手限流 |
| `bad_sid` | 400 | `sid` 不合法 |
| `draining` | 503 | 节点正在关闭 |
| `cluster_busy` | 503 | Redis 过慢，或等待中的会话登记过多 |
| `unknown_tenant` | 400 | 租户不存在 |
| `unsupported_protocol` | 400 | 没有提供受支持的子协议 |
| `origin_not_allowed` | 403 | 浏览器 Origin 不被允许 |
| `tenant_capacity` | 503 | 达到 `max_sessions_total` |
| `auth_missing` | 401 | 租户查找的位置上没有凭证 |
| `auth_failed` | 401 | 凭证被拒 |
| `auth_unavailable` | 503 | 认证后端或 JWKS 不可用 |

## 消息

| 指标 | 类型 | 标签 | 含义 |
|---|---|---|---|
| `wsprism_policy_decisions_total` | counter | `tenant`、`lane`、`decision`、`reason` | 被租户策略拒绝或限制的消息与会话 |
| `wsprism_dispatch_duration_seconds` | histogram | `tenant`、`lane` | 处理一条客户端消息的耗时（桶范围 100 µs 到 1 s；Hot 通道按 1/1024 采样） |
| `wsprism_decode_errors_total` | counter | `tenant` | 无法解码的客户端帧 |
| `wsprism_service_errors_total` | counter | `tenant`、`lane`、`svc` | 服务以错误回复的客户端消息 |

## 投递

| 指标 | 类型 | 含义 |
|---|---|---|
| `wsprism_egress_drop_total` | counter | 因接收方队列已满而丢弃的可丢弃帧 |
| `wsprism_egress_slow_consumer_closes_total` | counter | 因可靠帧放不进队列而关闭的会话 |
| `wsprism_writer_timeouts_total`（`tenant`） | counter | 因套接字写入阻塞超过 `writer_send_timeout_ms` 而关闭的会话 |
| `wsprism_hot_slot_overwritten_total` | counter | 发送前就被新值取代的带键 Hot 帧。属于正常现象；数值高说明读取方跟不上 |
| `wsprism_hot_slot_full_drops_total` | counter | 因接收方槽位表已满而丢弃的带键 Hot 帧 |

## 集群

| 指标 | 类型 | 含义 |
|---|---|---|
| `wsprism_broker_outbox_refused_total` | counter | 因通往 Redis 的队列已满或积压过久而被拒的帧；发布方收到 `UNAVAILABLE` |
| `wsprism_broker_retries_total` | counter | 因 Redis 缓慢或不可达而重试的 broker 调用 |
| `wsprism_broker_failed_total` | counter | 被 Redis 拒绝的帧或命令。这些都被丢弃了，需要排查 |

## 如何解读

- `wsprism_broker_retries_total` 在涨而拒绝数为零，说明 Redis 抖了一下，网关把它吸收了。出现拒绝，则意味着队列放弃了，而且发布方也得到了通知。
- `wsprism_broker_failed_total` 应当始终为零。常见原因是缺少某项 ACL 权限，或者在 `noeviction` 下 Redis 内存耗尽。
- 在有慢读取方的繁忙 Hot 房间里，`wsprism_egress_drop_total` 平时就会增长。如果只用 Ext 的租户出现了意外增长，请检查服务端 API 发布是否用了 `lossy`。
- 把 `wsprism_handshake_rejections_total{reason="auth_failed"}` 和 `auth_unavailable` 放在一起看：前者是用户的问题，后者是你的基础设施的问题。

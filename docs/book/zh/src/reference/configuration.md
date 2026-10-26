# 配置

网关读取一份 YAML 文件，路径通过 `--config` 或 `WSPRISM_CONFIG` 指定（默认 `./wsprism.yaml`）。本章列出全部配置项。常用配置项的带注释示例见仓库中的 [`wsprism.yaml`](https://github.com/AsyncNomad/wsPrism/blob/main/wsprism.yaml)。

```bash
wsprism-gateway --config wsprism.yaml --check-config
```

解析器很严格。未知字段、超出范围的值、相互矛盾的组合（空闲超时比心跳间隔还短、白名单里写了不存在的服务），都会在启动时报错并指出位置。`--check-config` 执行完全相同的检查，但不启动服务。

## 结构

```yaml
version: 1          # 必填；目前只有版本 1
gateway: { … }      # 进程级：监听地址、超时、broker
tenants: [ … ]      # 每个租户一项
```

## 从环境变量取值

任何值都可以写成 `${NAME}`（变量未设置或为空时启动失败）、`${NAME:-default}`；要表示字面量 `${`，写作 `$${`。

- 替换发生在解析之后，所以值永远无法改变文件的结构。键名不做替换。
- 如果一个值恰好只由一个占位符构成，替换结果能读成数字或布尔值时就取对应类型，否则为字符串。
- 在 `{ }` 或 `[ ]` 中请给占位符加引号（`"${QUEUE}"`），引号不影响类型推断。
- 敏感信息请优先用 `*_env` 类配置，它们在用到的地方才读取变量。

## `gateway`

| 配置项 | 默认值 | 范围 | 含义 |
|---|---|---|---|
| `listen` | `0.0.0.0:8080` | | 公网 WebSocket 监听地址 |
| `api.listen` | 关闭 | | 内部监听：服务端 API、`/metrics`、健康检查。请绑定回环地址或内网 |
| `public_metrics` | `false` | | 在公网监听上也提供 `/metrics` |
| `tls` | 关闭 | | 在 `listen` 上直接终止 TLS，见 [`gateway.tls`](#gatewaytls) |
| `ping_interval_ms` | 20000 | 5000~120000 | WebSocket 心跳间隔 |
| `idle_timeout_ms` | 60000 | 10000~600000，且大于心跳间隔 | 超过该时长没有任何入站活动即关闭连接 |
| `writer_send_timeout_ms` | 1500 | 50~60000 | 套接字写入阻塞超过该时长即关闭会话 |
| `drain_grace_ms` | 2000 | ≤ 600000 | 优雅停止的时限 |
| `session_queue_capacity` | 1024 | 1~1048576 | 每个会话的发送队列长度（按消息条数） |
| `socket.tcp_nodelay` | `true` | | 关闭 Nagle。请保持开启：Nagle 与延迟 ACK 叠加会让小帧多等几十毫秒 |
| `trusted_proxies` | `[]` | IP 或 CIDR | 可以通过 `X-Forwarded-For` 告知客户端地址的代理 |
| `log_identities` | `plain` | `plain`、`hashed`、`off` | 日志中用户 ID、会话 ID 和 IP 的呈现方式 |
| `log_identity_key_env` | 每个进程随机 | | 存放 `hashed` 所用 HMAC 密钥的变量（≥ 16 字节） |
| `broker` | `kind: memory` | | 见[集群 Broker](#集群-broker) |

### `gateway.tls`

公网监听上的 TLS（rustls：TLS 1.2 和 1.3、安全的密码套件、ALPN `http/1.1`、不要求客户端证书）。

| 配置项 | 默认值 | 范围 | 含义 |
|---|---|---|---|
| `cert_file` | 必填 | | PEM 证书链，叶子证书在前 |
| `key_file` | 必填 | | PEM 私钥（PKCS#8、PKCS#1 或 SEC1） |
| `handshake_timeout_ms` | 10000 | 100~60000 | 在此时间内未完成 TLS 握手的连接将被关闭 |

文件在启动时读取：文件缺失、格式错误或私钥与证书不匹配，都会导致启动和 `--check-config` 失败。收到 SIGHUP（或调用 `Gateway::reload_tls()`）时重新读取；新的握手使用新证书，读取失败则继续使用原来的证书。同时进行的握手最多 1,024 个。内部监听保持明文。

### `gateway.handshake_limit`

在 WebSocket 升级之前生效的令牌桶。被拒绝的请求收到 429 和 `Retry-After`。

| 配置项 | 默认值 | 含义 |
|---|---|---|
| `enabled` | `true` | 只有从单一地址做压测时才应关闭 |
| `global_rps` / `global_burst` | 100 / 200 | 所有客户端合计的每秒尝试次数 |
| `per_ip_rps` / `per_ip_burst` | 10 / 50 | 每个客户端地址的每秒尝试次数 |
| `ipv6_prefix_len` | 64（32~128） | IPv6 客户端按此前缀长度归组 |
| `max_ip_entries` | 50000 | 跟踪的地址数上限，超出后只受全局桶约束 |

## `tenants[]`

| 配置项 | 含义 |
|---|---|
| `id` | 1 到 64 个 `[A-Za-z0-9_-]` 字符，不得重复 |
| `limits` | 资源限额，见下文 |
| `policy` | 消息与会话策略，见下文 |
| `auth` | 认证链。省略时使用开发用认证器并打印警告 |
| `api_keys` | 服务端 API 密钥：`{name, sha256}` |
| `rpc` | 通往后端的 RPC 代理 |
| `history` | 房间历史规则 |

### `limits`

| 配置项 | 默认值 | 含义 |
|---|---|---|
| `max_frame_bytes` | 4096 | 客户端帧的最大字节数，在 WebSocket 编解码层强制执行（关闭码 1009） |
| `max_sessions_total` | 0（不限） | 租户并发会话数，按节点计 |
| `max_rooms_total` | 0（不限） | 至少有一名成员的房间数，按节点计 |
| `max_users_per_room` | 0（不限） | 每个房间的成员数，按节点计 |
| `max_rooms_per_user` | 64 | 一个用户可同时加入的房间数；0 表示不限 |

### `policy`

| 配置项 | 默认值 | 含义 |
|---|---|---|
| `ext_allowlist` | `["room:join", "room:leave"]` | 客户端可发送的 Ext 消息：`svc:type`、`svc:*` |
| `hot_allowlist` | `[]`（Hot 通道关闭） | 客户端可发送的 Hot 帧：`svc:op`、`svc:*` |
| `rate_limit_rps` / `rate_limit_burst` | 200 / 400 | 每秒入站消息数 |
| `rate_limit_scope` | `connection` | `connection`、`tenant`（共享额度）或 `both` |
| `sessions.mode` | `multi` | `single` 要求 `max_sessions_per_user: 1` |
| `sessions.max_sessions_per_user` | 4 | 每个用户的并发会话数（集群中跨节点计算） |
| `sessions.on_exceed` | `deny` | `deny` 拒绝新会话，或 `kick_oldest` 关闭最早的会话 |
| `allowed_origins` | `[]`（拒绝所有浏览器） | 精确的 Origin、`https://*.example.com` 或 `*` |
| `ticket_sources` | `[header, subprotocol]` | 客户端可放置凭证的位置，按优先级排列；`query` 需显式开启 |
| `hot_error_mode` | `sys_error` | `silent` 时丢弃被拒的 Hot 帧且不通知客户端 |
| `hot_requires_active_room` | `true` | 协议 v1：发送 Hot 帧前必须已加入房间 |
| `hot_echo_to_sender` | `true` | 把会话自己发出的 Hot 帧回显给它 |
| `hot_tick_hz` | 0（立即） | 1~240：每个会话的带键 Hot 状态每秒最多刷出的次数 |
| `presence_events` | `false` | 发送 `member_joined` / `member_left` 以及成员快照 |

白名单中出现的每个服务都必须存在：内置服务（`room`、`chat`、Hot `1`）、通过 `Gateway::builder` 注册的服务，或由 `rpc.allow` 路由的服务。

### `auth[]`

每一项都有一个 `kind`。

**`dev`**：仅供开发。接受 `dev`（用户 `dev`），以及 `dev.<id>` 或 `dev:<id>`（用户 `<id>`）。

**`ticket`**：由 `POST /api/v1/tickets` 签发的一次性票据，没有其他选项。

**`jwt`**：

| 配置项 | 默认值 | 含义 |
|---|---|---|
| `algorithm` | 必填 | `HS256/384/512`、`RS256/384/512`、`PS256/384/512`、`ES256/384`、`EdDSA` |
| `secret_file` / `secret_env` | | HMAC 密钥（HS*），长度不短于哈希长度 |
| `public_key_file` | | PEM 公钥（RS*、PS*、ES*、EdDSA） |
| `jwks_url` | | JWKS 端点，须为 `https`（明文 `http` 仅限回环地址） |
| `jwks_refresh_s` | 300（60~86400） | 后台刷新间隔 |
| `jwks_min_refetch_s` | 30（1~3600） | 因未知 `kid` 而重新拉取的最小间隔 |
| `jwks_max_stale_s` | 86400（1~604800） | 持续刷新失败时，超过此时长的密钥不再信任 |
| `issuer` / `audience` | | 设置后 `iss` / `aud` 为必需并会校验 |
| `leeway_s` | 30（≤ 300） | `exp` 和 `nbf` 允许的时钟偏差 |
| `require_tenant` | `false` | 要求令牌带有 `wsprism.tenant` 声明 |
| `kid` | | 只处理带有该密钥 ID 的令牌 |

**`proxy`**：

| 配置项 | 默认值 | 含义 |
|---|---|---|
| `url` | 必填 | 你的连接校验端点 |
| `timeout_ms` | 2000（1~60000） | 单次调用超时 |
| `max_in_flight` | 256 | 并发调用数，超出返回 503 |
| `secret_env` | | 以 `Authorization: Bearer <值>` 发送 |

**`plugin`**：`name`（用 `Gateway::builder().auth_plugin` 注册的名字）和 `options`（传给其工厂函数）。

### `api_keys[]`

`{ name: "backend", sha256: "<64 位十六进制>" }`。哈希可以用 `printf %s "$KEY" | sha256sum` 计算。

### `rpc`

| 配置项 | 默认值 | 含义 |
|---|---|---|
| `url` | 必填 | 你的 RPC 端点 |
| `allow` | 必填 | 转发给它的 `svc:type` / `svc:*` 模式；内置服务不可转发 |
| `timeout_ms` | 5000（1~60000） | 单次调用超时 |
| `max_in_flight` | 16 | 每条连接未完成的调用数，超出返回 `RATE_LIMITED` |
| `max_in_flight_total` | 1024 | 整个租户未完成的调用数，超出返回 `RESOURCE_EXHAUSTED` |
| `secret_env` | | 以 `Authorization: Bearer <值>` 发送 |
| `require_joined_room` | `true` | 拒绝指向本会话未加入房间的请求 |

### `history[]`

| 配置项 | 默认值 | 范围 | 含义 |
|---|---|---|---|
| `rooms` | 必填 | 房间名或 `prefix*` | 规则作用的房间；采用第一条匹配的规则 |
| `size` | 200 | 1~10000 | 每个房间保留的消息条数 |
| `ttl_s` | 600 | 1~86400 | 消息保留的秒数 |
| `max_bytes` | 1048576 | 1024~67108864 | 每个房间保留的字节数 |

## 集群 Broker

把 `gateway.broker` 设为 `kind: redis`，节点之间就通过 Redis 互联。见[集群部署](../guide/cluster.md)。

| 配置项 | 默认值 | 范围 | 含义 |
|---|---|---|---|
| `url` / `url_env` | 二者必填其一 | | `redis://` 或 `rediss://` URL，或保存该 URL 的变量 |
| `prefix` | `wsp` | 1~64 个 `[A-Za-z0-9_.-]` | 所有键和频道的前缀，多个集群可借此共用一个 Redis |
| `pubsub_connections` | 4 | 1~64 | 订阅连接数 |
| `slot_ttl_s` | 30 | 3~300 | 最后一次心跳后节点槽位的过期时间 |
| `outbox_capacity` | 65536 | | 待发往 Redis 的帧队列长度，所有租户合计 |
| `outbox_per_tenant` | 16384 | 1~capacity | 单个租户可排队的帧数 |
| `outbox_max_wait_ms` | 1000 | 10~60000 | 最早的帧等待超过此时长时，拒绝新的帧 |
| `tls.ca_file` | 系统证书库 | | 校验 Redis 证书所用的 CA |
| `tls.client_cert_file` / `tls.client_key_file` | | | 双向 TLS 的客户端证书 |
| `sentinel.addresses` | | | Sentinel 地址（与 Sentinel 之间走 TLS 时用 `rediss://`） |
| `sentinel.master_name` | | | Sentinel 中登记的主节点名称 |
| `replica_ack.replicas` | 配置 Sentinel 时为 1，否则关闭 | 0~16 | 登录成功前必须收到票据消耗记录的副本数 |
| `replica_ack.timeout_ms` | 500 | 1~10000 | 最长等待时间，超时则登录以 503 失败 |
| `admission.max_latency_ms` | 500 | 0（关闭）或 10~60000 | Redis 响应慢于此值期间，拒绝新连接 |
| `admission.max_pending_claims` | 10000 | 1~1000000 | 等待 Redis 的会话登记超过此数期间，拒绝新连接 |

## 重新加载

配置只在启动时读取一次。要修改就重启节点；多节点时逐个重启。

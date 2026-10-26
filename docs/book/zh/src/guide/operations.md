# 运维

本章写给在生产环境运行网关的人：如何启动和停止、会记录哪些日志、该关注哪些指标、该设置哪些告警。

## 命令行

| 参数 | 环境变量 | 默认值 | |
|---|---|---|---|
| `--config <path>` | `WSPRISM_CONFIG` | `wsprism.yaml` | 配置文件 |
| `--check-config` | | | 校验配置后退出 |
| `--log-format text\|json` | `WSPRISM_LOG_FORMAT` | `text` | 日志格式 |
| | `RUST_LOG` | 仅错误 | 日志级别，例如 `info` 或 `warn,wsprism_redis=info` |

请在 CI 和每次部署前运行 `--check-config`。它会解析配置、应用所有校验规则、构建每个租户的策略，包括加载 JWT 密钥、检查白名单里的服务是否存在。能通过检查的配置，就一定能启动。

## 一份配置，适配所有环境

任何配置值都可以取自环境变量：

```yaml
gateway:
  listen: "0.0.0.0:${PORT:-8080}"
  broker: { kind: redis, url_env: REDIS_URL, prefix: "${CLUSTER:-wsp}" }
```

`${NAME}` 在变量未设置或为空时会导致启动失败，`${NAME:-default}` 则回退到默认值。替换发生在解析之后，因此值永远无法改变文件的结构。敏感信息请优先使用 `*_env` 类配置（`secret_env`、`url_env`），它们在用到的地方才读取变量，值不会出现在错误信息里。

配置不支持热加载，修改后需要重启才能生效。多节点时逐个重启，即可在不停服的情况下应用新配置。

## 日志

日志输出到标准错误。`--log-format json` 每行输出一个对象，可以直接送进 Loki、Elasticsearch、Cloud Logging 等系统：

```json
{"ts":"2026-10-21T09:00:00.123456Z","level":"WARN","target":"wsprism_gateway::rpc",
 "msg":"rpc backend call failed","error":"Timeout",
 "url":"https://api.example.com/rpc?key=***",
 "span":{"trace_id":"5f0c…","t":"acme","u":"alice","s":"tab-1","ip":"203.0.113.7"}}
```

`span` 标明事件属于哪个会话：`trace_id`、租户（`t`）、用户（`u`）、会话 ID（`s`）、客户端地址（`ip`）、连接 ID（`c`）。同一个 `trace_id` 也会出现在发给客户端的错误里，用户反馈问题时可以直接对上网关日志。

以下内容从不写入日志：票据、令牌、API 密钥、后端凭证和消息正文。后端 URL 会去掉用户信息，查询参数的值替换为 `***`。

### 个人信息

在许多司法辖区，用户 ID、会话 ID 和 IP 地址都属于个人信息。它们在日志中如何呈现，由 `gateway.log_identities` 决定：

| 取值 | 日志中显示 |
|---|---|
| `plain`（默认） | 原值 |
| `hashed` | `h:` 加上 HMAC-SHA256 的 16 位十六进制 |
| `off` | 不显示（省略这些字段） |

使用 `hashed` 时，请把 `gateway.log_identity_key_env` 设为一个保存着至少 16 字节密钥的环境变量，并在所有节点上使用同一把密钥。这样同一个人在任何节点、重启前后都对应同一个哈希值；要查找某个用户的会话，用同一把密钥对其 ID 求哈希即可。不设置密钥时，每个进程会随机生成一把。

## 停止与升级

收到 SIGTERM（或 Ctrl-C）后开始排空：

1. `/readyz` 变为 503，`wsprism_draining` 变为 1。
2. 新连接和服务端 API 发布以 503 拒绝。
3. 会话以 4103 `draining`（协议 v1 为 1001）关闭，客户端稍作等待后重连到其他节点。
4. 集群节点把队列中的帧交给 Redis。
5. 最迟在 `gateway.drain_grace_ms`（默认 2 秒）后进程退出。

编排系统的停止等待时间要比 `drain_grace_ms` 长。多节点时请参考[滚动升级](cluster.md#滚动升级)。

## TLS 证书

配置 `gateway.tls` 后，网关自己提供 `wss://`。轮换证书时，替换文件后发送 `SIGHUP` 即可：新的握手使用新证书，已打开的连接不受影响；重新加载失败会记录日志，并继续使用原来的证书。Let's Encrypt 这类需要频繁续期的证书，也可以用这种方式更新而不断开任何人。

## 容器

仓库根目录的 `Dockerfile` 构建一个 distroless 镜像：以非 root 用户运行、不含 shell，并设置了 `WSPRISM_LOG_FORMAT=json`。把配置文件挂载到 `/etc/wsprism/wsprism.yaml`，并让 `gateway.listen` 和 `gateway.api.listen` 绑定 `0.0.0.0`，回环地址在容器外是访问不到的。

```bash
docker build -t wsprism-gateway .
docker run -v $PWD/my.yaml:/etc/wsprism/wsprism.yaml:ro -p 8080:8080 wsprism-gateway
```

镜像里没有 `curl`，请使用编排系统自带的 HTTP 探针。镜像中包含 CA 证书，网关调用 `https` 后端或 JWKS 地址时需要它们；如果自己构建精简镜像，记得加上。

## 健康检查

| 接口 | 返回 | 位置 |
|---|---|---|
| `/healthz` | 进程在运行时返回 200 | 公网与内部监听 |
| `/readyz` | 排空期间、或集群节点失去槽位后返回 503 | 公网与内部监听 |
| `/metrics` | Prometheus 文本格式 | 内部监听；仅在 `gateway.public_metrics: true` 时也出现在公网监听上 |

## 指标

完整列表（含类型和标签）见[指标](../reference/metrics.md)。标签值只来自配置或固定集合，从不取自客户端输入，因此时间序列的数量不会超过租户数量所决定的上限。

最先要看的几项：

- 按租户的 `wsprism_ws_sessions_active`：突然下跌说明客户端正在被断开。
- 按原因的 `wsprism_handshake_rejections_total`：`rate_limit`、`tenant_capacity`、`auth_failed`、`auth_unavailable`、`origin_not_allowed`、`cluster_busy` 等，告诉你用户为什么连不进来。
- `wsprism_egress_slow_consumer_closes_total`：跟不上可靠流量的客户端。持续增长意味着队列偏小，或者某些房间发布的量超出了客户端的读取能力。
- `wsprism_dispatch_duration_seconds`：处理一条客户端消息所需的时间。
- 集群下的 `wsprism_broker_outbox_refused_total` 和 `wsprism_broker_failed_total`：因 Redis 饱和而被拒的发布，以及被 Redis 拒绝的命令。

## 告警示例

```yaml
- alert: WsPrismSessionsDropped
  expr: sum(wsprism_ws_sessions_active) < 0.5 * sum(wsprism_ws_sessions_active offset 10m)
- alert: WsPrismBrokerRefusing
  expr: rate(wsprism_broker_outbox_refused_total[5m]) > 0
- alert: WsPrismBrokerFailures
  expr: rate(wsprism_broker_failed_total[5m]) > 0
- alert: WsPrismSlowConsumers
  expr: rate(wsprism_egress_slow_consumer_closes_total[10m]) > 1
```

另外为每个节点配置一条针对 `/readyz` 的探针告警。

## 容量估算

以下数据在一台桌面级 CPU 上测得，网关占用 8 个核心（见[基准测试](../project/benchmarks.md)）：

- **内存：** 每条连接约 18 到 38 KB 进程内存，另加内核套接字缓冲区；上限对应的是所有房间都开启历史的情况。每个带历史的房间最多再占用其 `max_bytes`。
- **CPU：** 视房间形态而定，每次投递约 3 到 8 µs；由于写入会合并，负载越高这个数字越低。每秒投递两百万次时，p99 延迟仍低于 10 ms。
- **连接风暴：** 集群中每个节点每秒约 3 万个新会话。

承诺具体数字之前，请用自己的实际流量测一测。消息大小、房间规模以及 Hot 与 Ext 的比例，都会让这些数字有明显出入。

## 安全加固清单

- 通过 `wss://` 提供服务：配置 `gateway.tls`，或在独立的负载均衡器上终止 TLS。避免在同一台主机上放置 TLS 代理，它会让每次投递的 CPU 翻倍。
- 替换掉 `dev` 认证器；没有配置 `auth` 的租户会在启动时打印警告。
- 服务端 API 只放在回环地址或内网上，每个后端使用独立的密钥。
- 在 `allowed_origins` 中写明确切的 Origin。
- 部署在负载均衡器后面时，配置 `trusted_proxies`，让按客户端限流看到真实地址。
- 没有旧客户端的话，`ticket_sources` 保持默认值。
- 为每个租户设置 `limits`：帧大小、会话数、房间数。
- 集群：使用按[集群部署](cluster.md)中 ACL 配置的专用 Redis 用户、启用 TLS、设置 `noeviction`。
- 考虑启用 `log_identities: hashed`。

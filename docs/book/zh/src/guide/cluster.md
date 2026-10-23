# 集群部署

一个 wsPrism 节点就能扛住非常多的连接，但只有一个节点，也就意味着一次故障就是一次停服。使用 Redis broker 后，多个节点共享房间、会话、在线状态和历史消息，客户端连到哪个节点都可以。

> **状态：**集群功能已经齐备，也通过了下文的各项故障测试，但在经过生产环境检验之前仍标记为**实验性**。0.x 版本中它的配置可能还会调整。

## 需要部署什么

- **网关节点**，放在任意负载均衡器后面，不需要会话粘滞。也可以不用负载均衡器，把节点列表交给客户端（SDK 中的 `url: [...]`），每次重连尝试下一个。
- **一个 Redis，6.2 或更新版本**，可以是单个地址，也可以是由 Redis Sentinel 管理的主节点。暂不支持 Redis Cluster；键名已经使用了哈希标签，将来支持时不需要改名。
- **健康检查**：存活检查用 `/healthz`，就绪检查用 `/readyz`。失去集群身份的节点会变成未就绪，应当重启。

配置如下：

```yaml
gateway:
  broker:
    kind: redis
    url_env: WSPRISM_REDIS_URL     # 例如 rediss://wsprism:secret@redis.internal:6380/0
    prefix: wsp
```

其他可调参数都有合理的默认值，完整列表见[配置](../reference/configuration.md#集群-broker)。仓库中的 `deploy/compose/` 用 Docker Compose 启动两个节点和一个 Redis，所有节点共用一份配置文件，差异全部来自环境变量。

### 准备 Redis

- 设置 `maxmemory-policy noeviction`（或某种 `volatile-*` 策略）。如果 Redis 驱逐了网关的键，带历史的房间会以新的 epoch 重新开始，客户端必须重新同步。
- 持久化可有可无，而且它并不能让历史消息跨越 Redis 重启（见下文 [Redis 重启之后](#redis-重启之后)）。
- 创建一个最小权限用户。经过测试验证的完整命令列表如下：

  ```
  ACL SETUSER wsprism on >PASSWORD ~wsp:* &wsp:* -@all +publish +subscribe +unsubscribe +set +get +getdel +del +pexpire +time +zadd +zcard +zrange +zrangebyscore +zremrangebyrank +zpopmin +hget +hmget +hgetall +hset +hincrby +hdel +hexists +sadd +srem +smembers +info +eval +evalsha +script|load +client|setinfo
  ```

  把 `wsp` 换成你的前缀。`client|setinfo` 可以不给。

- 在你无法完全掌控的网络上请使用 TLS（`rediss://`）。私有 CA 和客户端证书在 `gateway.broker.tls` 中配置。

### Sentinel

要跟随 Sentinel 的主从切换，列出各个 Sentinel：

```yaml
gateway:
  broker:
    kind: redis
    url_env: REDIS_URL       # 只用其中的凭证和库号；主机由 Sentinel 告知
    sentinel:
      addresses: ["redis://sentinel-1:26379", "redis://sentinel-2:26379", "redis://sentinel-3:26379"]
      master_name: mymaster
```

节点每秒向 Sentinel 询问一次主节点，一旦变化就把所有连接迁移到新主节点。测试中，主节点停止约 4.5 秒后投递恢复；全链路启用双向 TLS 时为 2.3 到 3.2 秒。Sentinel 地址写成 `rediss://` 即通过 TLS 连接 Sentinel，`tls` 配置同时作用于 Sentinel 和 Redis 节点。

使用 Sentinel 时，消耗一次性票据要等到至少一个副本收到这次写入（`replica_ack`，配置 Sentinel 时默认开启）。这是主从切换可能撤销、却又没有任何机制能修复的唯一一种写入：如果被提升的副本没收到这次消耗，就会把同一张票据再接受一遍。超时内没有副本确认，这次登录就以 503 失败。想在主从切换期间也能正常用票据登录，请部署两个副本。

## 节点如何协作

- **启动。** 节点在接待任何人之前，先在 Redis 中占用 1,024 个*节点槽位*中的一个，并用心跳续约。成员 ID 中包含槽位号，因此在整个集群中唯一。连不上 Redis 的节点不会启动，由编排系统负责重启。
- **房间流量。** Ext 消息都经过 Redis，所以每个节点投递同一房间消息的顺序完全一致。Hot 状态先直接投递给本地成员，同时发往其他节点。
- **会话**在集群范围内统一管理。用同一个 `sid` 重连，会顶替旧连接，不管它在哪个节点上；`max_sessions_per_user` 也在所有节点间精确生效：由 Redis 脚本裁决，两条并发连接不可能同时胜出。
- **在线状态**也是集群范围的：在线状态接口和 `sys.joined` 快照会列出所有节点上的成员。
- **历史消息**存放在 Redis 中。Lua 脚本在存储并发布消息的同时为它分配 offset，因此所有节点看到的 offset 一致，客户端在任何节点上都能续收。
- **自我修复。** 每个节点每 15 秒与 Redis 核对一次，把 Redis 丢失的会话和成员关系写回去。

## 出现以下情况时

下表每一行都在混沌测试或随机浸泡测试中实际触发并验证过。

| 情况 | 客户端的感受 | 恢复 |
|---|---|---|
| 节点被强杀 | 其客户端重连到其他节点，带历史的房间从最后的 offset 续收：不丢、不重 | 在 `slot_ttl_s` 加约 5 秒内，其他节点移除它的成员，并发送一次 `member_left` |
| 节点正常停止（SIGTERM） | `/readyz` 变为 503，会话以 4103 `draining` 关闭并在别处续上；不丢、不重 | 在 `drain_grace_ms` 内把队列交给 Redis，然后退出 |
| Redis 以空数据或较旧快照重启 | 带历史的房间开启新 epoch（带 epoch 的 `sys.gap`），其他房间收到 `sys.gap`；没有人被断开 | 节点重连、重新订阅、重新加载脚本，约 15 秒内恢复会话 |
| Sentinel 主从切换 | 同 Redis 重启 | 节点约 1 秒内跟上 |
| Redis 变慢 | 发布排队等待；超过 `outbox_max_wait_ms`（1 秒）后，新的发布得到 `UNAVAILABLE`。超过 `admission.max_latency_ms`（500 ms）后，新连接得到 503 `cluster_busy` 和 1~5 秒的 `Retry-After`；已连接的会话不受影响 | Redis 恢复后，在一次 250 ms 的探测内自动解除 |
| Redis 宕机 | 新连接以 4102 `unavailable` 关闭；可靠发布得到 `UNAVAILABLE`；票据登录得到 503；同一节点上的成员之间 Hot 状态照常流动 | Redis 恢复后自动解除 |
| 节点与 Redis 断开超过 `slot_ttl_s` | 在其他节点看来，它的房间暂时是空的 | 重新连上后重新注册并恢复会话。如果期间槽位已被别的节点占用，它会排空并以错误退出，由编排系统重启 |
| 节点之间的关闭命令丢失 | 被顶替的会话可能暂时残留 | 自我修复会在约 15 秒内关闭它 |

### Redis 重启之后

重启后的 Redis 或被提升的副本是另一个进程，其数据可能比客户端已经看到的还要旧。放任不管，同一个 offset 就可能对应不同的内容。网关通过 Redis 的 `run_id` 察觉这种变化，让所有带历史的房间开启新的 epoch，并通过 `sys.gap` 告知客户端。客户端于是重新同步，而不是在不知情的情况下收到被复用的 offset。快照本来就比最后的写入要旧，所以开不开持久化都一样。

### Fencing（隔离）

一个断开太久、槽位已被别人接管的节点，不能若无其事地继续发布。这件事由 Redis 来保证：房间发布在执行它的同一个脚本里校验槽位归属，槽位不属于自己的节点，每一次发布都会被拒绝。该节点会在第一次被拒的写入或下一次心跳（以先到者为准）时排空并退出。会话和成员关系的写入不受 fencing 约束，节点消失后由其他节点清理。

## 滚动升级

一次只重启一个节点：

1. 发送 SIGTERM（Kubernetes 删除 Pod 时会自动发送）。节点变为未就绪，以 503 拒绝新连接和服务端 API 发布，以 4103 关闭会话，并把队列交给 Redis，全部在 `drain_grace_ms`（默认 2 秒）内完成。
2. 启动新版本，等待 `/readyz` 返回 200。
3. 处理下一个节点。

在 Kubernetes 上：`terminationGracePeriodSeconds` 要大于 `drain_grace_ms`，就绪探针指向 `/readyz`，并设置 `maxUnavailable: 1`。混沌测试严格按这个流程执行，并检查没有任何消息丢失或重复。

## 投递语义

- **同一房间内，所有人顺序一致。** 所有节点按相同顺序投递一个房间的 Ext 消息。
- **不同房间之间，没有全局顺序。** 发往不同房间的消息，离开节点的顺序可能与发布顺序不同。
- **跨节点时，以 Redis 收到的顺序为准。** 如果后端把消息 A 发给节点 1、把 B 发给节点 2，成员按 Redis 收到它们的顺序接收。若一个房间内消息的先后很重要，请通过同一个节点发布。
- **服务端 API 返回 200 意味着"已进入该节点的队列"。** 节点如果在清空队列之前（通常只需几微秒）被直接杀掉，队列中的内容会丢失；以 SIGTERM 停止的节点会先把队列交出去。
- **网关自身的重试不会让历史消息重复。** 每一帧都带有按节点编号的序号，由 Redis 脚本检查。不带历史的房间消息只有一种情况会重复：Redis 已经执行了发布，但在回复之前连接断了。
- **后端自己的重试可能造成重复。** 如果一次发布超时，你又通过另一个节点重发，而第一个节点其实已经收下了，成员就会收到两份。对此敏感的话，请在消息里放一个 ID。

## 容量

目前的集群提供的是**高可用，以及有限的扩展**。节点越多，能承载的连接和 Hot 流量越多；但房间历史受限于单个 Redis 核心。

| 流量 | 实测上限 |
|---|---|
| Hot 状态 | 与单节点相同（先在本地投递） |
| 不带历史的 Ext | 每个节点每秒约 24 万次发布全部送达；Redis 每次发布约耗 2.5 µs，单核下整个集群每秒约 60 万次 |
| 带历史的 Ext | 约 200 字节的消息，每个 Redis 核心每秒约 15 万条 |
| 连接风暴 | 每个节点每秒约 3 万个新会话；对最坏情况（连接、加入、离开、断开），单个 Redis 核心每秒约能处理 6 万个 |

请让 `gateway.session_queue_capacity` 保持默认值（1024）或更大：来自 Redis 的帧是成批到达的，队列太小会把正常的读取方也当作慢消费者断开。

## 已知限制

- 租户级限额（`max_sessions_total` 以及各项房间限额）按节点分别计算。
- 只有一个 Redis 主节点。把房间流量分散到多个 Redis 实例的方案已经设计好，但尚未实现。
- Redis 以空数据重启后，如果旧节点还没来得及恢复会话（约 15 秒），用户就用同一个 `sid` 在另一个节点上重连，可能会短暂出现同一 sid 有两个会话的情况。

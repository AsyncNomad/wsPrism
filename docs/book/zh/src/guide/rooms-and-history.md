# 房间、在线状态与历史消息

实时应用的大部分时间都花在房间里：一场对局、一份文档、一个聊天频道、一只股票。本章介绍如何加入和离开房间、如何知道还有谁在，以及断线后如何补上错过的消息。

## 加入与离开

```json
{"v":1,"svc":"room","type":"join","room":"match:42","seq":1}
{"v":1,"svc":"sys","type":"joined","room":"match:42","chan":17,"member_id":4097,"reply_to":1}

{"v":1,"svc":"room","type":"leave","room":"match:42","seq":2}
{"v":1,"svc":"sys","type":"left","room":"match:42","reply_to":2}
```

满足以下所有条件时才能加入：

- 租户允许 `room:join`；
- 如果用户带有 `subscribe` 权限，该权限覆盖这个房间；
- 租户限额还有余量：整个租户的 `max_rooms_total`、该房间的 `max_users_per_room`、该用户的 `max_rooms_per_user`（默认 64，因为每加入一个房间都要在若干索引里占用内存）。

触及限额时返回 `RESOURCE_EXHAUSTED`。重复加入同一个房间没有副作用，也不会发出任何通知。

`sys.joined` 回复中有两个在 Hot 通道上很重要的标识：`chan` 是该房间在这条连接上的数字 ID，`member_id` 是这次成员身份作为 Hot 帧发送方时的标识。请把两者都当作不透明的值，并且预期重连后它们会变。

会话无论因为什么原因关闭，都会退出它所在的全部房间。

## 发布

内置的 `chat` 服务会把消息广播到发送者已加入的房间：

```json
{"v":1,"svc":"chat","type":"send","room":"match:42","data":{"msg":"gg"}}
```

你自己的服务通过上下文发布，后端则通过服务端 API（`POST /api/v1/publish`）发布。来自服务端 API 的消息到达成员时的格式是 `{"svc":"server","type":"publish","room":…,"data":…}`。

在同一个房间里，所有成员收到 Ext 消息的顺序都相同，在集群中不论成员连在哪个节点上也是如此。

## 在线状态

想知道房间里有谁，有两种方式：

- **从后端查询：** `GET /api/v1/rooms/{room}/presence` 返回房间内的用户列表（每个用户只出现一次，集群中汇总所有节点）。
- **由客户端接收：** 开启 `policy.presence_events: true` 后，有人进出时会通知房间成员，加入房间的回复里也会带上已经在场的成员。

```json
{"v":1,"svc":"sys","type":"member_joined","room":"lobby","data":{"user":"bob","member_id":7}}
{"v":1,"svc":"sys","type":"member_left","room":"lobby","data":{"user":"bob","member_id":7}}
```

离开房间的每一种途径都会发出 `member_left`：主动离开、断线、被挤下线、被服务端踢下线；在集群中，成员所在的节点宕机也算（在该节点的 `slot_ttl_s` 再加几秒之内，恰好发送一次）。`sys.joined` 中的成员快照最多 1,000 人，超出时会标记 `members_truncated`。

在线状态事件默认关闭，因为每次进出都要广播给整个房间。一个一万人的房间里，这会为大多数成员根本不看的信息付出不少流量。只在"谁在场"真正重要的房间里开启，比如对局、文档、小型频道。

`member_id` 与该成员所发 Hot 帧中的 `sender` 字段相同，客户端不需要额外记账，就能移除已离开玩家的头像。

## 历史消息

房间可以保留最近的消息，让断线重连的客户端收到错过的内容。历史消息按租户配置，规则按房间名匹配，采用第一条匹配的规则；不匹配任何规则的房间不保留任何消息。

```yaml
tenants:
  - id: acme
    history:
      - { rooms: "chat:*", size: 200, ttl_s: 600 }
      - { rooms: "orders", size: 1000, ttl_s: 3600, max_bytes: 4194304 }
```

| 字段 | 默认值 | 范围 | 含义 |
|---|---|---|---|
| `rooms` | | 房间名或 `prefix*` | 规则作用的房间 |
| `size` | 200 | 1~10,000 | 每个房间保留的消息条数 |
| `ttl_s` | 600 | 1~86,400 | 消息保留的秒数 |
| `max_bytes` | 1 MiB | 1 KiB~64 MiB | 每个房间保留的字节数，超出时先删最旧的 |

内存预算按 `max_bytes` 乘以可能匹配的房间数来估算（房间数受 `max_rooms_total` 约束）。单节点时历史消息放在网关内存里，集群时放在 Redis 里。

### offset 与 epoch

带历史的房间里，每条消息都有一个 `offset`，从 1 开始，每保留一条消息加一。offset 只在某个 **epoch** 内有意义；epoch 是一个不透明的 ID，代表这个房间历史的一段生命周期。`sys.joined` 会报告当前位置：

```json
{"svc":"sys","type":"joined","room":"chat:1","epoch":"9f2c…","offset":44}
```

在线状态事件、Hot 帧和非 JSON 消息没有 offset，也不会被保留。

### 断点续收

要接着收，就带上客户端最后看到的位置重新加入：

```json
{"v":1,"svc":"room","type":"join","room":"chat:1","seq":3,"data":{"recover":{"epoch":"9f2c…","offset":41}}}
{"v":1,"svc":"sys","type":"joined","room":"chat:1","reply_to":3,"epoch":"9f2c…","offset":44,"recovered":true}
```

- `"recovered": true`：第 42 到 44 条消息会紧跟在回复之后、按顺序、先于任何新消息送达。
- `"recovered": false`：这段空缺补不上。可能是 epoch 变了（网关重启、Redis 丢了数据、房间空置超过 `ttl_s`），也可能是消息已经因为 `size`、`max_bytes` 或 `ttl_s` 被清掉了。请从后端重新加载房间状态。

不带 `recover` 的加入请求，回复里没有 `recovered` 字段。客户端应当丢弃 offset 已经见过的消息；JavaScript SDK 正是这么做的，并且每次重连都会自动续收。

慢消费者也靠同样的方式恢复。因 `slow_consumer` 被断开的客户端重连后，从最后的 offset 开始续收，补发的量不受当初溢出的那个队列大小限制。

### 空缺

`sys.gap` 告诉客户端，某个房间的消息流出现了断档：

```json
{"v":1,"svc":"sys","type":"gap","room":"lobby"}
{"v":1,"svc":"sys","type":"gap","room":"chat:1","epoch":"a71e…"}
```

不带 `epoch` 时，表示该房间可能丢了消息：在集群中，节点曾短暂失去 Redis 订阅。请重新加载房间状态。带 `epoch` 时，表示房间的历史本身重新开始了（Redis 以空数据或较旧的快照恢复）。offset 会在新 epoch 中从头计数，所以要忘掉这个房间最后看到的 offset，否则新的、更小的 offset 看起来就像重复消息。SDK 会处理这两种情况，并触发 `gap` 事件。

## 历史消息不是什么

历史消息用来弥合重连的间隙，不是存储。聊天记录、订单、文档这类重要数据，请以你自己的数据库为准，并把 `recovered: false` 和 `sys.gap` 当作从那里重新加载的信号。要让网关保证持久投递，就得有预写日志和副本复制，那是数据库的活儿，而且会让每条消息都背上 wsPrism 本来要消除的那份延迟。

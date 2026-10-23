# Hot 通道

Hot 通道承载那些变化太快、没人需要看到每一个中间值的状态：坐标、光标、瞄准方向、调音台上的推子、最优买价。它放弃了 Ext 通道的投递保证，换来低延迟和有上限的队列。

## 帧格式

Hot 帧是 WebSocket 二进制消息。在协议 v2 下，客户端发送：

```
[kind=0x01][flags u8][chan u32 LE][svc u8][op u8][key u32 LE?][seq u32 LE?][payload…]
```

接收时结构相同，只是在负载前插入了发布者的 `sender u32`。可选字段是否存在由标志位决定：

| 标志 | 字段 | 含义 |
|---|---|---|
| `0x01` | `key` | 这是该键的状态，新值取代旧值 |
| `0x02` | `seq` | 序号，接收方用它丢弃乱序到达的帧 |
| `0x04` | `sender` | 仅由服务端设置，即发布者的 `member_id` |

`chan` 是 `sys.joined` 中的房间 ID。指向本会话未加入房间的帧会被丢弃并计数（`policy_decisions{reason="unknown_channel"}`）。`hot_error_mode: sys_error` 时还会通知客户端；设为 `silent` 则省下这份带宽。

负载对网关是不透明的，按你的应用需要编码即可：几个浮点数、FlatBuffer、protobuf 都行。

协议 v1 客户端使用一种更早、更简单的头部（`[v][svc][op][flags][seq?][payload]`），发往它的*活动房间*，即最后加入的那个房间。两种客户端可以待在同一个房间里，v1 接收方只会收到负载。

## 路由与授权

Hot 帧由 `svc` 所标识的服务处理。服务 `1` 是内置的**房间转发服务**，会把帧转发给房间里的其他成员。你自己的 Hot 服务在 Rust 中通过 `Gateway::builder().hot_service(...)` 注册。

租户的 `policy.hot_allowlist` 列出客户端可以发送的 `svc:op` 组合，例如 `"1:*"` 表示转发服务的全部操作码。列表为空（默认）时，该租户的 Hot 通道处于关闭状态。列出的每个服务都必须真实存在，否则无法启动。

## 键与合并

让 Hot 通道不只是一根"可丢包的管子"的，正是键。带键的帧是*状态*：当某个接收方的队列积压时，网关只为它保留每个 `(sender, svc, key)` 的最新一帧，被取代的旧帧直接丢掉。卡了半秒的客户端，收到的是房间里每个人的当前位置，而不是每人三十个过时的位置。

每一份相互独立的状态用一个键：比如玩家控制的每个实体一个，或者每个光标一个。不带键的帧被视为事件，逐条投递；接收方的队列满了，就只对该接收方丢弃。

合并和丢弃发生了多少，可以看两个计数器：`wsprism_hot_slot_overwritten_total`（发送前就被新值取代的状态；属于正常现象，数值高说明读取方跟不上）和 `wsprism_egress_drop_total`。

## 序号

接收方应丢弃那些 `seq` 比同一发送方、同一键最后见到的序号更旧的帧。WebSocket 上帧会按序到达，但基于数据报的传输可能乱序。JavaScript SDK 已经替你处理了这一点（包括 `u32` 回绕），发送时也会按键编号。

## 用 tick 控制节奏

默认情况下，带键的状态一到就发。设置 `policy.hot_tick_hz`（1 到 240）后，每个会话积压的带键状态每秒最多刷出这么多次；每个会话的刷出时刻各有错开，不会挤在同一瞬间。当更新频率高于 tick 时，这能同时省下 CPU 和带宽，代价是最多多一个 tick 的延迟。不带键的帧不受节奏控制。

光标和休闲游戏可以从 30 Hz 起步，动作类游戏用 60 Hz 比较合适。

## 回显给发送者

`policy.hot_echo_to_sender`（默认 `true`）决定会话是否会经由转发服务收到自己发出的帧。状态同步类客户端通常本来就知道自己的状态，设为 `false` 可以省下房间 Hot 流量的大约 1/N。

## 从后端发布

服务端 API 也能发布 Hot 帧：

```json
POST /api/v1/publish
{"room": "match:42", "hot": {"svc": 1, "op": 2, "key": 0, "data_b64": "AAEC"}}
```

这类帧的发送方是 `0`。在集群中，它们会送达所有节点上的成员。

## 现阶段"可丢弃"的含义

在 WebSocket 上，Hot 帧只是*在网关内部*可丢弃。帧一离开服务器就走 TCP，丢一个包，后面所有帧都得等它重传（队头阻塞）。网络干净时这没有任何代价；在有丢包的移动网络上，它会表现为卡顿。

真正的不可靠投递需要数据报。我们在单向延迟 20 ms、丢包 1% 的模拟链路上，以 60 Hz 对比了 WebTransport 数据报和基于 TLS 的 WebSocket：状态的额外陈旧度（超额 AoI）降到原来的三分之一到四分之一，明显过时的帧占比从约 3% 降到几乎为零，代价是每条连接的 CPU 和内存大约翻倍。因此 WebTransport 的规划是作为小型交互房间的一种选择，大型广播房间继续用 WebSocket。详见[基准测试](../project/benchmarks.md)和[路线图](../project/roadmap.md)。

## 示例

在文档房间里共享光标，30 Hz 发送，不把用户自己的光标回显给自己：

```yaml
tenants:
  - id: docs
    policy:
      ext_allowlist: ["room:*"]
      hot_allowlist: ["1:1"]
      hot_tick_hz: 30
      hot_echo_to_sender: false
      presence_events: true
```

```js
await client.join("doc:7f3a");
window.addEventListener("pointermove", (e) => {
  const buf = new Float32Array([e.clientX, e.clientY]);
  client.sendState("doc:7f3a", { svc: 1, op: 1, key: 0, payload: new Uint8Array(buf.buffer) });
});
client.on("state", (s) => drawCursor(s.sender, new Float32Array(s.payload.slice().buffer)));
client.on("presence", (p) => p.type === "left" && removeCursor(p.memberId));
```

每个用户鼠标一动就发；观看的一方对每个光标每秒最多收到 30 次更新，而且永远是最新的那一个。
